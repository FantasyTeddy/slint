/*! module for the C++ code generator
*/

/// This module contains some datastructure that helps represent a C++ code.
/// It is then rendered into an actual C++ text using the Display trait
mod cpp_ast {

    use std::cell::Cell;
    use std::fmt::{Display, Error, Formatter};
    thread_local!(static INDETATION : Cell<u32> = Cell::new(0));
    fn indent(f: &mut Formatter<'_>) -> Result<(), Error> {
        INDETATION.with(|i| {
            for _ in 0..(i.get()) {
                write!(f, "    ")?;
            }
            Ok(())
        })
    }

    ///A full C++ file
    #[derive(Default, Debug)]
    pub struct File {
        pub includes: Vec<String>,
        pub declarations: Vec<Declaration>,
    }

    impl Display for File {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), Error> {
            for i in &self.includes {
                writeln!(f, "#include {}", i)?;
            }
            for d in &self.declarations {
                write!(f, "\n{}", d)?;
            }
            Ok(())
        }
    }

    /// Declarations  (top level, or within a struct)
    #[derive(Debug, derive_more::Display)]
    pub enum Declaration {
        Struct(Struct),
        Function(Function),
        Var(Var),
    }

    #[derive(Default, Debug)]
    pub struct Struct {
        pub name: String,
        pub members: Vec<Declaration>,
    }

    impl Display for Struct {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), Error> {
            indent(f)?;
            writeln!(f, "struct {} {{", self.name)?;
            INDETATION.with(|x| x.set(x.get() + 1));
            for m in &self.members {
                // FIXME! identation
                write!(f, "{}", m)?;
            }
            INDETATION.with(|x| x.set(x.get() - 1));
            indent(f)?;
            writeln!(f, "}};")
        }
    }

    /// Function or method
    #[derive(Default, Debug)]
    pub struct Function {
        pub name: String,
        /// "(...) -> ..."
        pub signature: String,
        /// The function does not have return type
        pub is_constructor: bool,
        pub is_static: bool,
        /// The list of statement instead the function.  When None,  this is just a function
        /// declaration without the definition
        pub statements: Option<Vec<String>>,
    }

    impl Display for Function {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), Error> {
            indent(f)?;
            if self.is_static {
                write!(f, "static ")?;
            }
            if !self.is_constructor {
                write!(f, "auto ")?;
            }
            write!(f, "{} {}", self.name, self.signature)?;
            if let Some(st) = &self.statements {
                writeln!(f, "{{")?;
                for s in st {
                    indent(f)?;
                    writeln!(f, "    {}", s)?;
                }
                indent(f)?;
                writeln!(f, "}}")
            } else {
                writeln!(f, ";")
            }
        }
    }

    /// A variable or a member declaration.
    #[derive(Default, Debug)]
    pub struct Var {
        pub ty: String,
        pub name: String,
        pub init: Option<String>,
    }

    impl Display for Var {
        fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), Error> {
            indent(f)?;
            write!(f, "{} {}", self.ty, self.name)?;
            if let Some(i) = &self.init {
                write!(f, " = {}", i)?;
            }
            writeln!(f, ";")
        }
    }

    pub trait CppType {
        fn cpp_type(&self) -> Result<&str, crate::diagnostics::CompilerDiagnostic>;
    }
}

use crate::diagnostics::{CompilerDiagnostic, Diagnostics};
use crate::object_tree::{Component, Element, PropertyDeclaration};
use crate::typeregister::Type;
use cpp_ast::*;

impl CppType for PropertyDeclaration {
    fn cpp_type(&self) -> Result<&str, CompilerDiagnostic> {
        match self.property_type {
            Type::Float32 => Ok("float"),
            Type::Int32 => Ok("int"),
            Type::String => Ok("sixtyfps::SharedString"),
            Type::Color => Ok("uint32_t"),
            Type::Bool => Ok("bool"),
            _ => Err(CompilerDiagnostic {
                message: "Cannot map property type to C++".into(),
                span: self.type_location.clone(),
            }),
        }
    }
}

fn handle_item(
    item: &Element,
    global_properties: &Vec<String>,
    main_struct: &mut Struct,
    init: &mut Vec<String>,
) {
    main_struct.members.push(Declaration::Var(Var {
        ty: format!("sixtyfps::{}", item.base_type.as_builtin().class_name),
        name: item.id.clone(),
        ..Default::default()
    }));

    let id = &item.id;
    init.extend(item.bindings.iter().map(|(s, i)| {
        use crate::expression_tree::Expression;
        match i {
            Expression::SignalReference { component:_, element:_, name } => {
                let signal_accessor_prefix = if item.signals_declaration.contains(s) {
                    String::new()
                } else {
                    format!("{id}.", id = id.clone())
                };

                format!(
                    "{signal_accessor_prefix}{prop}.set_handler([](const void *root) {{ reinterpret_cast<const {ty}*>(root)->{fwd}.emit(root); }});",
                    signal_accessor_prefix = signal_accessor_prefix, prop = s, fwd = name.clone(), ty = main_struct.name
                )
            }
            _ => {
                let accessor_prefix = if item.property_declarations.contains_key(s) {
                    String::new()
                } else {
                    format!("{id}.", id = id.clone())
                };

                let init = compile_expression(i);
                format!(
                    "{accessor_prefix}{cpp_prop}.set({init});",
                    accessor_prefix = accessor_prefix,
                    cpp_prop = s,
                    init = init
                )
            }
        }
    }));

    for i in &item.children {
        handle_item(&i.borrow(), global_properties, main_struct, init)
    }
}

/// Returns the text of the C++ code produced by the given root component
pub fn generate(component: &Component, diag: &mut Diagnostics) -> Option<impl std::fmt::Display> {
    let mut x = File::default();

    x.includes.push("<sixtyfps.h>".into());

    let mut main_struct = Struct { name: component.id.clone(), ..Default::default() };

    let mut declared_property_members = vec![];
    let mut declared_property_vars = vec![];
    for (cpp_name, property_decl) in component.root_element.borrow().property_declarations.iter() {
        let cpp_type = property_decl.cpp_type().unwrap_or_else(|err| {
            diag.push_compiler_error(err);
            "".into()
        });

        declared_property_members.push(cpp_name.clone());
        declared_property_vars.push(Declaration::Var(Var {
            ty: format!("sixtyfps::Property<{}>", cpp_type),
            name: cpp_name.clone(),
            init: None,
        }));
    }

    main_struct.members.extend(declared_property_vars);

    let mut init = Vec::new();
    handle_item(
        &component.root_element.borrow(),
        &declared_property_members,
        &mut main_struct,
        &mut init,
    );

    main_struct.members.extend(component.root_element.borrow().signals_declaration.iter().map(
        |s| Declaration::Var(Var { ty: "sixtyfps::Signal".into(), name: s.clone(), init: None }),
    ));

    main_struct.members.push(Declaration::Function(Function {
        name: component.id.clone(),
        signature: "()".to_owned(),
        is_constructor: true,
        statements: Some(init),
        ..Default::default()
    }));

    main_struct.members.push(Declaration::Function(Function {
        name: "tree_fn".into(),
        signature: "(sixtyfps::ComponentRef) -> const sixtyfps::ItemTreeNode* ".into(),
        is_static: true,
        ..Default::default()
    }));

    main_struct.members.push(Declaration::Var(Var {
        ty: "static const sixtyfps::ComponentVTable".to_owned(),
        name: "component_type".to_owned(),
        init: None,
    }));

    x.declarations.push(Declaration::Struct(main_struct));

    let mut tree_array = String::new();
    super::build_array_helper(component, |item: &Element, children_offset| {
        tree_array = format!(
            "{}{}sixtyfps::make_item_node(offsetof({}, {}), &sixtyfps::{}, {}, {})",
            tree_array,
            if tree_array.is_empty() { "" } else { ", " },
            &component.id,
            item.id,
            item.base_type.as_builtin().vtable_symbol,
            item.children.len(),
            children_offset,
        )
    });

    x.declarations.push(Declaration::Function(Function {
        name: format!("{}::tree_fn", component.id),
        signature: "(sixtyfps::ComponentRef) -> const sixtyfps::ItemTreeNode* ".into(),
        statements: Some(vec![
            "static const sixtyfps::ItemTreeNode children[] {".to_owned(),
            format!("    {} }};", tree_array),
            "return children;".to_owned(),
        ]),
        ..Default::default()
    }));

    x.declarations.push(Declaration::Var(Var {
        ty: "const sixtyfps::ComponentVTable".to_owned(),
        name: format!("{}::component_type", component.id),
        init: Some("{ nullptr, sixtyfps::dummy_destory, tree_fn }".to_owned()),
    }));

    if diag.has_error() {
        None
    } else {
        Some(x)
    }
}

fn compile_expression(e: &crate::expression_tree::Expression) -> String {
    use crate::expression_tree::Expression::*;
    match e {
        StringLiteral(s) => format!(r#"sixtyfps::SharedString("{}")"#, s.escape_default()),
        NumberLiteral(n) => n.to_string(),
        PropertyReference { name, .. } => format!(r#"{}.get(nullptr)"#, name),
        Cast { from, to } => {
            let f = compile_expression(&*from);
            match (from.ty(), to) {
                (Type::Float32, Type::String) | (Type::Int32, Type::String) => {
                    format!("sixtyfps::SharedString::from_number({})", f)
                }
                _ => f,
            }
        }
        _ => format!("\n#error: unsupported expression {:?}\n", e),
    }
}
