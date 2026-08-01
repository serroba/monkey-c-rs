//! A scope tree over a parsed file, used to answer "where was this name declared?".
//!
//! The tree mirrors Monkey C's nesting — a document holds modules and classes, which hold functions
//! — and records, for every declaration, the span of its *name* rather than of the whole
//! declaration, because that is what an editor wants to select when it jumps.
//!
//! Function bodies are one flat scope. Monkey C scopes a `var` to its enclosing block, but treating
//! the whole body as a single scope only differs when a name is declared twice in sibling blocks,
//! and answering with the wrong one of two same-named locals is a far smaller error than the extra
//! machinery costs.

use monkey_c_parser::ast::{
    Ast, Binding, BlockStmt, ElseBranch, ForInit, IfStmt, Span, Spanned, Stmt,
};

/// What introduced a scope. Modules and classes are named containers reachable through a qualified
/// path (`Combo.beats`); functions are not.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ScopeKind {
    Document,
    Module,
    Class,
    Function,
}

#[derive(Debug, PartialEq)]
pub struct Decl {
    pub name: String,
    pub name_span: Span,
}

#[derive(Debug, PartialEq)]
pub struct Scope {
    pub kind: ScopeKind,
    pub name: Option<String>,
    pub span: Span,
    pub decls: Vec<Decl>,
    pub children: Vec<Scope>,
}

impl Scope {
    /// The declaration of `name` directly in this scope.
    pub fn decl(&self, name: &str) -> Option<&Decl> {
        self.decls.iter().find(|decl| decl.name == name)
    }

    /// The named child container (module or class) called `name`.
    pub fn container(&self, name: &str) -> Option<&Scope> {
        self.children
            .iter()
            .find(|child| child.is_container() && child.name.as_deref() == Some(name))
    }

    /// Collect every named container at or below this scope into `found`, outermost first.
    pub fn containers_named<'a>(&'a self, name: &str, found: &mut Vec<&'a Scope>) {
        if self.is_container() && self.name.as_deref() == Some(name) {
            found.push(self);
        }

        for child in &self.children {
            child.containers_named(name, found);
        }
    }

    /// The chain of scopes containing `offset`, outermost first. Always starts with this scope.
    pub fn chain_at(&self, offset: usize) -> Vec<&Scope> {
        let mut chain = vec![self];
        let mut scope = self;

        while let Some(child) = scope
            .children
            .iter()
            .find(|child| contains(child.span, offset))
        {
            chain.push(child);
            scope = child;
        }

        chain
    }

    fn is_container(&self) -> bool {
        matches!(self.kind, ScopeKind::Module | ScopeKind::Class)
    }
}

/// Whether `span` covers `offset`. The end is inclusive so a caret resting immediately after the
/// last character of a name still counts as being on it.
pub fn contains(span: Span, offset: usize) -> bool {
    span.start <= offset && offset <= span.end
}

/// Build the scope tree for a parsed document.
pub fn scope_tree(ast: &Ast) -> Scope {
    let empty = Span { start: 0, end: 0 };
    let (nodes, span) = match ast {
        Ast::Document(nodes, span) => (nodes.as_slice(), *span),
        _ => (std::slice::from_ref(ast), *ast.span().unwrap_or(&empty)),
    };

    let mut document = Scope {
        kind: ScopeKind::Document,
        name: None,
        span,
        decls: Vec::new(),
        children: Vec::new(),
    };
    collect_body(nodes, &mut document);

    document
}

/// Record every declaration in `nodes` into `scope`, recursing into the containers they introduce.
fn collect_body(nodes: &[Ast], scope: &mut Scope) {
    for node in nodes {
        match node {
            Ast::Module(decl) => {
                scope.decls.push(named(&decl.name));

                let mut child = container(ScopeKind::Module, &decl.name, decl.span);
                collect_body(&decl.body, &mut child);
                scope.children.push(child);
            }
            Ast::Class(decl) => {
                scope.decls.push(named(&decl.name));

                let mut child = container(ScopeKind::Class, &decl.name, decl.span);
                collect_body(&decl.body, &mut child);
                scope.children.push(child);
            }
            Ast::Function(decl) => {
                scope.decls.push(named(&decl.name));

                let mut child = Scope {
                    kind: ScopeKind::Function,
                    name: Some(decl.name.node.clone()),
                    span: decl.span,
                    decls: decl.parameters.iter().map(|p| named(&p.name)).collect(),
                    children: Vec::new(),
                };
                if let Some(body) = &decl.body {
                    collect_block(body, &mut child);
                }

                scope.children.push(child);
            }
            Ast::Const(decl) => scope.decls.extend(bindings(&decl.bindings)),
            Ast::Variable(decl) => scope.decls.extend(bindings(&decl.bindings)),
            Ast::Typedef(decl) => scope.decls.push(named(&decl.name)),
            Ast::Enum(decl) => {
                if let Some(name) = &decl.name {
                    scope.decls.push(named(name));
                }

                // Variants are reachable unqualified (anonymous enums) or through the enclosing
                // module (`Movement.TYPE_MILL`), and recording them beside the enum resolves both
                // spellings.
                for variant in &decl.variants {
                    scope.decls.push(Decl {
                        name: variant.name.clone(),
                        name_span: Span {
                            start: variant.span.start,
                            end: variant.span.start + variant.name.len(),
                        },
                    });
                }
            }
            Ast::Document(nodes, _) => collect_body(nodes, scope),
            Ast::Annotation(..) | Ast::Import(_) | Ast::Using(_) | Ast::Eof => {}
        }
    }
}

/// Record the locals a function body declares. Nested blocks feed the same scope — see the module
/// comment on why block scoping is flattened.
fn collect_block(block: &BlockStmt, scope: &mut Scope) {
    for stmt in &block.stmts {
        collect_stmt(stmt, scope);
    }
}

fn collect_stmt(stmt: &Stmt, scope: &mut Scope) {
    match stmt {
        Stmt::Var(decl) => scope.decls.extend(bindings(&decl.bindings)),
        Stmt::Block(block) => collect_block(block, scope),
        Stmt::If(stmt) => collect_if(stmt, scope),
        Stmt::While(stmt) => collect_block(&stmt.body, scope),
        Stmt::DoWhile(stmt) => collect_block(&stmt.body, scope),
        Stmt::For(stmt) => {
            if let Some(ForInit::Var(decl)) = &stmt.header.init {
                scope.decls.extend(bindings(&decl.bindings));
            }

            collect_block(&stmt.body, scope);
        }
        Stmt::Switch(stmt) => {
            for case in &stmt.cases {
                for stmt in &case.stmts {
                    collect_stmt(stmt, scope);
                }
            }
        }
        Stmt::Try(stmt) => {
            collect_block(&stmt.body, scope);
            for catch in &stmt.catches {
                collect_block(&catch.body, scope);
            }

            if let Some(finally) = &stmt.finally {
                collect_block(finally, scope);
            }
        }
        Stmt::Break(_) | Stmt::Continue(_) | Stmt::Return(_) | Stmt::Throw(_) | Stmt::Expr(_) => {}
    }
}

fn collect_if(stmt: &IfStmt, scope: &mut Scope) {
    collect_block(&stmt.then_branch, scope);

    match &stmt.else_branch {
        Some(ElseBranch::Block(block)) => collect_block(block, scope),
        Some(ElseBranch::If(nested)) => collect_if(nested, scope),
        None => {}
    }
}

fn container(kind: ScopeKind, name: &Spanned<String>, span: Span) -> Scope {
    Scope {
        kind,
        name: Some(name.node.clone()),
        span,
        decls: Vec::new(),
        children: Vec::new(),
    }
}

fn named(name: &Spanned<String>) -> Decl {
    Decl {
        name: name.node.clone(),
        name_span: name.span,
    }
}

fn bindings(bindings: &[Binding]) -> Vec<Decl> {
    bindings
        .iter()
        .map(|binding| named(&binding.name))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use monkey_c_parser::parser::Parser;

    fn tree(source: &str) -> Scope {
        scope_tree(
            &Parser::new(source)
                .parse()
                .expect("source should parse")
                .ast,
        )
    }

    fn names(scope: &Scope) -> Vec<&str> {
        scope.decls.iter().map(|decl| decl.name.as_str()).collect()
    }

    #[test]
    fn module_members_land_in_the_module_scope() {
        let tree = tree("module Combo {\n const HAND_LEFT = 0;\n function beats() {}\n}\n");
        let combo = tree
            .container("Combo")
            .expect("module should be a container");

        assert_eq!(names(&tree), vec!["Combo"]);
        assert_eq!(names(combo), vec!["HAND_LEFT", "beats"]);
    }

    #[test]
    fn parameters_and_locals_land_in_the_function_scope() {
        let source = "function cycleBeats(pattern) {\n var total = 0;\n}\n";
        let tree = tree(source);
        let function = &tree.children[0];

        assert_eq!(function.kind, ScopeKind::Function);
        assert_eq!(names(function), vec!["pattern", "total"]);
    }

    #[test]
    fn locals_in_nested_blocks_reach_the_function_scope() {
        let source = "function f() {\n for (var i = 0; i < 2; i++) {\n var inner = 1;\n }\n}\n";
        let function = &tree(source).children[0];

        assert_eq!(names(function), vec!["i", "inner"]);
    }

    #[test]
    fn else_if_branches_are_walked() {
        let source =
            "function f() {\n if (a) {\n var x = 1;\n } else if (b) {\n var y = 2;\n }\n}\n";
        let function = &tree(source).children[0];

        assert_eq!(names(function), vec!["x", "y"]);
    }

    #[test]
    fn enum_variants_are_declared_beside_the_enum() {
        let tree = tree("module Movement {\n enum { TYPE_MILL, TYPE_BULLWHIP }\n}\n");
        let movement = tree
            .container("Movement")
            .expect("module should be a container");

        assert_eq!(names(movement), vec!["TYPE_MILL", "TYPE_BULLWHIP"]);
    }

    #[test]
    fn a_declaration_points_at_its_name() {
        let source = "module Combo {\n function beats() {}\n}\n";
        let tree = tree(source);
        let combo = tree
            .container("Combo")
            .expect("module should be a container");
        let beats = combo.decl("beats").expect("function should be declared");

        assert_eq!(&source[beats.name_span.start..beats.name_span.end], "beats");
    }

    #[test]
    fn the_scope_chain_reaches_the_innermost_function() {
        let source = "module Combo {\n function beats() {\n var total = 0;\n }\n}\n";
        let tree = tree(source);
        let offset = source.find("total").expect("local should be present");

        let kinds: Vec<_> = tree.chain_at(offset).iter().map(|s| s.kind).collect();
        assert_eq!(
            kinds,
            vec![ScopeKind::Document, ScopeKind::Module, ScopeKind::Function]
        );
    }
}
