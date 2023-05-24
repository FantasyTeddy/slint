// Copyright © SixtyFPS GmbH <info@slint-ui.com>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-commercial

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::Sample;

use futures::future::OptionFuture;
use futures::FutureExt;
use ringbuf::HeapRb;

use super::ControlCommand;

pub struct AudioPlaybackThread {
    control_sender: smol::channel::Sender<ControlCommand>,
    packet_sender: smol::channel::Sender<ffmpeg_next::codec::packet::packet::Packet>,
    receiver_thread: Option<std::thread::JoinHandle<()>>,
}

impl AudioPlaybackThread {
    pub fn start(stream: &ffmpeg_next::format::stream::Stream) -> Result<Self, anyhow::Error> {
        let (control_sender, control_receiver) = smol::channel::unbounded();

        let (packet_sender, packet_receiver) = smol::channel::bounded(128);

        let decoder_context = ffmpeg_next::codec::Context::from_parameters(stream.parameters())?;
        let mut packet_decoder = decoder_context.decoder().audio()?;

        let host = cpal::default_host();
        let device = host.default_output_device().expect("no output device available");

        let config = device.default_output_config().unwrap();

        if config.sample_format() != cpal::SampleFormat::F32 {
            return Err(anyhow::format_err!("Only f32 audio output is implemented right now, but your host audio system uses a different format"));
        }

        let receiver_thread =
            std::thread::Builder::new().name("audio playback thread".into()).spawn(move || {
                smol::block_on(async move {
                    let output_channel_layout = match config.channels() {
                        1 => ffmpeg_next::util::channel_layout::ChannelLayout::MONO,
                        2 => {
                            ffmpeg_next::util::channel_layout::ChannelLayout::STEREO_LEFT
                                | ffmpeg_next::util::channel_layout::ChannelLayout::STEREO_RIGHT
                        }
                        _ => todo!(),
                    };

                    let output_format = ffmpeg_next::util::format::sample::Sample::F32(
                        ffmpeg_next::util::format::sample::Type::Packed,
                    );

                    let mut resampler = ffmpeg_next::software::resampling::Context::get(
                        packet_decoder.format(),
                        packet_decoder.channel_layout(),
                        packet_decoder.rate(),
                        output_format,
                        output_channel_layout,
                        config.sample_rate().0,
                    )
                    .unwrap();

                    let buffer = HeapRb::new(4096);
                    let (mut sample_producer, mut sample_consumer) = buffer.split();

                    let cpal_stream = device
                        .build_output_stream(
                            &config.config(),
                            move |data: &mut [f32], _| {
                                let filled = sample_consumer.pop_slice(data);
                                data[filled..].fill(f32::EQUILIBRIUM);
                            },
                            move |err| {
                                eprintln!("error feeding audio stream to cpal: {}", err);
                            },
                            None,
                        )
                        .unwrap();

                    cpal_stream.play().unwrap();

                    let packet_receiver_impl = async {
                        loop {
                            let Ok(packet) = packet_receiver.recv().await else { break };

                            packet_decoder.send_packet(&packet).unwrap();

                            let mut decoded_frame = ffmpeg_next::util::frame::Audio::empty();

                            while packet_decoder.receive_frame(&mut decoded_frame).is_ok() {
                                let mut resampled_frame = ffmpeg_next::util::frame::Audio::empty();
                                resampler.run(&decoded_frame, &mut resampled_frame).unwrap();

                                // Audio::plane() returns the wrong slice size, so correct it by hand. See also
                                // for a fix https://github.com/zmwangx/rust-ffmpeg/pull/104.
                                let expected_bytes = resampled_frame.samples()
                                    * resampled_frame.channels() as usize
                                    * core::mem::size_of::<f32>();
                                let cpal_sample_data: &[f32] = bytemuck::cast_slice(
                                    &resampled_frame.data(0)[..expected_bytes],
                                );

                                while sample_producer.free_len() < cpal_sample_data.len() {
                                    smol::Timer::after(std::time::Duration::from_millis(16)).await;
                                }

                                // Buffer the samples for playback
                                sample_producer.push_slice(cpal_sample_data);
                            }
                        }
                    }
                    .fuse()
                    .shared();

                    let mut playing = true;

                    loop {
                        let packet_receiver: OptionFuture<_> =
                            if playing { Some(packet_receiver_impl.clone()) } else { None }.into();

                        smol::pin!(packet_receiver);

                        futures::select! {
                            _ = packet_receiver => {},
                            received_command = control_receiver.recv().fuse() => {
                                match received_command {
                                    Ok(ControlCommand::Pause) => {
                                        playing = false;
                                    }
                                    Ok(ControlCommand::Play) => {
                                        playing = true;
                                    }
                                    Err(_) => {
                                        // Channel closed -> quit
                                        return;
                                    }
                                }
                            }
                        }
                    }
                })
            })?;

        Ok(Self { control_sender, packet_sender, receiver_thread: Some(receiver_thread) })
    }

    pub async fn receive_packet(&self, packet: ffmpeg_next::codec::packet::packet::Packet) -> bool {
        match self.packet_sender.send(packet).await {
            Ok(_) => return true,
            Err(smol::channel::SendError(_)) => return false,
        }
    }

    pub async fn send_control_message(&self, message: ControlCommand) {
        self.control_sender.send(message).await.unwrap();
    }
}

impl Drop for AudioPlaybackThread {
    fn drop(&mut self) {
        self.control_sender.close();
        if let Some(receiver_join_handle) = self.receiver_thread.take() {
            receiver_join_handle.join().unwrap();
        }
    }
}