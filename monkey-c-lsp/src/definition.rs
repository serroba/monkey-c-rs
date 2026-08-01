//! Resolving a caret position to the declaration it refers to.
//!
//! The name under the caret is read from the token stream rather than the AST. Tokens still exclude
//! comments and string bodies, but they survive a syntax error anywhere else in the file — so
//! navigation keeps working while a file is mid-edit, which is exactly when it is wanted. The
//! answer still comes from parsed sources: only the *question* is lexical.
//!
//! Symbols from the Connect IQ SDK (`Toybox.*`) resolve to nothing, since the server indexes the
//! workspace and the SDK is not part of it.

use std::path::Path;

use gen_lsp_types::Location;
use monkey_c_parser::ast::{Ast, Span};
use monkey_c_parser::lexer::Lexer;
use monkey_c_parser::parser::Parser;
use monkey_c_parser::token;

use crate::position::PositionMapper;
use crate::symbols::{Scope, scope_tree};
use crate::uri;
use crate::workspace::Workspace;

/// Every declaration the caret at `offset` could refer to.
///
/// More than one is returned when a qualified name matches in several files — Monkey C merges
/// same-named modules across a project, so both halves are honest answers and the editor offers
/// the choice.
pub fn definition(workspace: &Workspace, path: &Path, offset: usize) -> Vec<Location> {
    let Some(text) = workspace.text(path) else {
        return Vec::new();
    };

    let Some(segments) = reference_at(text, offset) else {
        return Vec::new();
    };

    let Some((name, qualifiers)) = segments.split_last() else {
        return Vec::new();
    };

    if qualifiers.is_empty() {
        return unqualified(workspace, path, text, name, offset);
    }

    qualified(workspace, qualifiers, name)
}

/// A bare name: the enclosing scopes decide, innermost first, so a parameter shadows a module
/// member of the same name. Only if nothing encloses it does the rest of the workspace get a say.
fn unqualified(
    workspace: &Workspace,
    path: &Path,
    text: &str,
    name: &str,
    offset: usize,
) -> Vec<Location> {
    if let Some(ast) = parse(text) {
        let tree = scope_tree(&ast);

        for scope in tree.chain_at(offset).into_iter().rev() {
            if let Some(decl) = scope.decl(name) {
                return vec![location(path, text, decl.name_span)];
            }
        }
    }

    top_level(workspace, name)
}

/// A dotted name: the leading segments name containers to descend through, and the last is looked
/// up among the members of whatever they arrive at.
fn qualified(workspace: &Workspace, qualifiers: &[String], name: &str) -> Vec<Location> {
    let mut locations = Vec::new();

    for (path, text) in workspace.sources() {
        let Some(ast) = parse(text) else {
            continue;
        };

        let tree = scope_tree(&ast);
        let mut roots = Vec::new();
        tree.containers_named(&qualifiers[0], &mut roots);

        for root in roots {
            let Some(container) = descend(root, &qualifiers[1..]) else {
                continue;
            };

            if let Some(decl) = container.decl(name) {
                locations.push(location(path, text, decl.name_span));
            }
        }
    }

    locations
}

/// Names declared at the top level of any source — the modules and classes a file can refer to
/// without qualifying them.
fn top_level(workspace: &Workspace, name: &str) -> Vec<Location> {
    let mut locations = Vec::new();

    for (path, text) in workspace.sources() {
        let Some(ast) = parse(text) else {
            continue;
        };

        if let Some(decl) = scope_tree(&ast).decl(name) {
            locations.push(location(path, text, decl.name_span));
        }
    }

    locations
}

fn descend<'a>(scope: &'a Scope, segments: &[String]) -> Option<&'a Scope> {
    let mut scope = scope;
    for segment in segments {
        scope = scope.container(segment)?;
    }

    Some(scope)
}

/// The dotted name the caret sits in, ending with the segment it is actually on: a caret on `Combo`
/// in `Combo.beats` asks about the module, not the member.
fn reference_at(text: &str, offset: usize) -> Option<Vec<String>> {
    let tokens = tokens(text);
    let position = tokens.iter().position(|(start, token, end)| {
        matches!(token, token::Type::Identifier(_)) && *start <= offset && offset <= *end
    })?;

    let token::Type::Identifier(name) = &tokens[position].1 else {
        return None;
    };

    let mut segments = vec![name.clone()];
    let mut index = position;
    while index >= 2
        && matches!(tokens[index - 1].1, token::Type::Dot)
        && let token::Type::Identifier(qualifier) = &tokens[index - 2].1
    {
        segments.insert(0, qualifier.clone());
        index -= 2;
    }

    Some(segments)
}

fn tokens(text: &str) -> Vec<(usize, token::Type, usize)> {
    let mut lexer = Lexer::new(text);
    let mut tokens = Vec::new();

    loop {
        let (start, token, end) = lexer.next_token();
        if matches!(token, token::Type::Eof) {
            break;
        }

        // A token that consumes nothing leaves the cursor where it was, so a lexer that fails to
        // advance would spin this loop forever. Stopping is the safe reading: the tokens so far
        // still answer for everything ahead of the offending byte.
        if end == start {
            break;
        }

        tokens.push((start, token, end));
    }

    tokens
}

fn parse(text: &str) -> Option<Ast> {
    Parser::new(text).parse().ok().map(|output| output.ast)
}

fn location(path: &Path, text: &str, span: Span) -> Location {
    Location {
        uri: uri::from_path(path),
        range: PositionMapper::new(text).range(span),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const COMBO: &str = "\
module Combo {
    const HAND_LEFT = 0;

    function beats() as Array<Number> {
        return [1, 2];
    }

    function cycleBeats(pattern as Array<Number>) as Number {
        var total = 0;
        total += pattern[0];

        return total;
    }
}
";

    const TEST: &str = "\
import Toybox.Test;

function testCombo(logger as Test.Logger) as Boolean {
    var beats = Combo.beats();
    Test.assertEqualMessage(Combo.cycleBeats(beats), 10, \"spans ten\");

    return true;
}
";

    fn workspace() -> Workspace {
        let mut workspace = Workspace::default();
        workspace.set(PathBuf::from("/src/Combo.mc"), COMBO.to_string());
        workspace.set(PathBuf::from("/src/ComboTest.mc"), TEST.to_string());

        workspace
    }

    /// Resolve the caret sitting on `needle` within `haystack`, returning the text each answer
    /// points at so assertions read as "it jumped to this name".
    fn goto(file: &str, source: &str, needle: &str) -> Vec<String> {
        let workspace = workspace();
        let offset = source.find(needle).expect("needle should be present");

        definition(&workspace, &PathBuf::from(file), offset)
            .into_iter()
            .map(|location| {
                let path = uri::to_path(&location.uri).expect("answer should be a file");
                let text = workspace
                    .text(&path)
                    .expect("answer should be a known source")
                    .to_string();
                let mapper = PositionMapper::new(&text);
                let start = mapper.offset(location.range.start);
                let end = mapper.offset(location.range.end);

                format!(
                    "{}:{}",
                    path.file_name().unwrap().to_string_lossy(),
                    &text[start..end]
                )
            })
            .collect()
    }

    #[test]
    fn a_parameter_resolves_to_itself() {
        assert_eq!(
            goto("/src/Combo.mc", COMBO, "pattern["),
            vec!["Combo.mc:pattern"]
        );
    }

    #[test]
    fn a_local_shadows_everything_outside_the_function() {
        assert_eq!(
            goto("/src/Combo.mc", COMBO, "total +="),
            vec!["Combo.mc:total"]
        );
    }

    #[test]
    fn a_module_member_resolves_across_files() {
        assert_eq!(
            goto("/src/ComboTest.mc", TEST, "beats()"),
            vec!["Combo.mc:beats"]
        );
    }

    #[test]
    fn a_qualifier_resolves_to_its_module() {
        assert_eq!(
            goto("/src/ComboTest.mc", TEST, "Combo.beats"),
            vec!["Combo.mc:Combo"]
        );
    }

    #[test]
    fn a_local_wins_over_a_same_named_module_member() {
        // `beats` is both a local in the test and a function in Combo; the local is nearer.
        assert_eq!(
            goto("/src/ComboTest.mc", TEST, "beats), 10"),
            vec!["ComboTest.mc:beats"]
        );
    }

    #[test]
    fn sdk_symbols_resolve_to_nothing() {
        assert!(goto("/src/ComboTest.mc", TEST, "assertEqualMessage").is_empty());
        assert!(goto("/src/ComboTest.mc", TEST, "Test.Logger").is_empty());
    }

    #[test]
    fn a_caret_in_a_string_resolves_to_nothing() {
        assert!(goto("/src/ComboTest.mc", TEST, "spans ten").is_empty());
    }

    #[test]
    fn a_broken_file_still_resolves_across_files() {
        let mut workspace = workspace();
        let broken = "function f() { var x = Combo.beats(; }\n";
        workspace.set(PathBuf::from("/src/Broken.mc"), broken.to_string());

        let offset = broken.find("beats(").expect("needle should be present");
        let found = definition(&workspace, &PathBuf::from("/src/Broken.mc"), offset);

        assert_eq!(
            found.len(),
            1,
            "the unparseable file should not block the jump"
        );
    }
}
