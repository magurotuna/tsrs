//! Rule execution contracts on native tsrs compiler objects.

use tsrs_ast::{Kind, Node, SourceFile};
use tsrs_checker::Checker;
use tsrs_compiler::Program;
use tsrs_core::P;
use tsrs_spanmap::Segment;

use crate::engine::{self, Options};
use crate::protocol::{Diagnostic, Fix, Range, RuleMessage, Suggestion};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Listener {
    Enter(Kind),
    Exit(Kind),
    PatternEnter(Kind),
    PatternExit(Kind),
    ExpressionEnter(Kind),
    ExpressionExit(Kind),
}

pub struct RuleContext<'a> {
    pub source_file: P<SourceFile>,
    pub program: &'static Program,
    pub checker: &'a mut Checker,
    pub rule_name: &'a str,
    pub options: Options,
    pub diagnostics: &'a mut Vec<Diagnostic>,
    /// The span map segments of a content-mapped file, sorted by their virtual start. `None` for a file that no
    /// content mapper produced.
    pub segments: Option<&'a [Segment]>,
}

impl RuleContext<'_> {
    /// Whether a content mapper wrote all of the text in `range` of the linted text: the file is content-mapped, the
    /// range is not empty, no part of it is a verbatim copy of the original file, and it contains no whole atom or
    /// alias that stands for original text (`engine::is_mapper_written`). Text in a gap between the segments, such as
    /// the `throw` that replaces a `return`, and text inside a larger atom, such as an event handler wrapper that is
    /// attributed to its tag name, is text that the mapper wrote. Ranges are half-open, so a segment that only touches
    /// an end of the range does not count, and neither does an empty segment.
    ///
    /// A rule calls this for the syntax that its verdict depends on when that syntax is outside the report range.
    /// The engine checks only the report range (`original_lint_range`). Pass the range of one token: a range that
    /// has one verbatim byte is not mapper-written. This is `false` for each range of a file without a content
    /// mapper, so a rule does not need to check the file kind first. Do not use it for every keyword: a mapper also
    /// writes keywords for constructs that the user wrote, such as the `if` of a Vue `v-if`. Text inside an atom can
    /// also be the user's: the Vue mapper maps the whole value of a component prop as one atom to the prop name.
    pub fn is_mapper_written(&self, range: Range) -> bool {
        self.segments.is_some_and(|segments| engine::is_mapper_written(segments, range))
    }

    /// Whether a content mapper wrote the first token of `node`: the keyword of a statement such as `throw` or
    /// `return`, or a token node such as the `=>` of an arrow function. Leading trivia is skipped, because a mapper
    /// can keep the user's indentation and comments in front of a keyword that it wrote.
    pub fn is_first_token_mapper_written(&self, node: P<Node>) -> bool {
        if self.segments.is_none() {
            return false;
        }
        let range = first_token_range(self.source_file, node);
        let mapper_written = self.is_mapper_written(range);
        // `TSRS_LINT_DEBUG_MAPPING`, as in `engine::lint_one`: a skipped node gives no report that could be printed.
        if mapper_written && std::env::var_os("TSRS_LINT_DEBUG_MAPPING").is_some() {
            eprintln!(
                "tsrs-lint mapping: {} skipped a node: the mapper wrote its first token on virtual [{}, {}) {:?}",
                self.rule_name,
                range.pos,
                range.end,
                self.source_file
                    .text()
                    .get(range.pos as usize..range.end as usize)
                    .unwrap_or_default(),
            );
        }
        mapper_written
    }

    pub fn report_range(&mut self, range: Range, message: RuleMessage) {
        self.report_diagnostic(Diagnostic { range: Some(range), message, ..Default::default() });
    }

    pub fn report_node(&mut self, node: P<Node>, message: RuleMessage) {
        self.report_range(trim_node_range(self.source_file, node), message);
    }

    pub fn report_diagnostic(&mut self, mut diagnostic: Diagnostic) {
        diagnostic.rule = Some(self.rule_name.into());
        diagnostic.file_path = Some(self.source_file.file_name().into());
        self.diagnostics.push(diagnostic);
    }

    pub fn report_node_with_fixes(
        &mut self,
        node: P<Node>,
        message: RuleMessage,
        fixes: impl FnOnce() -> Vec<Fix>,
    ) {
        let fixes = if self.options.fix { fixes() } else { Vec::new() };
        self.report_diagnostic(Diagnostic {
            range: Some(trim_node_range(self.source_file, node)),
            message,
            fixes,
            ..Default::default()
        });
    }

    pub fn report_node_with_suggestions(
        &mut self,
        node: P<Node>,
        message: RuleMessage,
        suggestions: impl FnOnce() -> Vec<Suggestion>,
    ) {
        let suggestions = if self.options.fix_suggestions { suggestions() } else { Vec::new() };
        self.report_diagnostic(Diagnostic {
            range: Some(trim_node_range(self.source_file, node)),
            message,
            suggestions,
            ..Default::default()
        });
    }
}

pub trait Rule {
    fn initialize(&mut self, _context: &mut RuleContext<'_>) {}

    /// Listener membership must remain stable after initialization for this file.
    fn listens_to(&self, listener: Listener) -> bool;
    fn visit(&mut self, listener: Listener, node: P<Node>, context: &mut RuleContext<'_>);
}

pub fn trim_node_range(file: P<SourceFile>, node: P<Node>) -> Range {
    Range {
        pos: tsrs_scanner::get_range_of_token_at_position(
            file,
            node.pos().expect_source("lint nodes are parsed source nodes"),
        )
        .source_pos()
        .unwrap(),
        end: node.end().expect_source("lint nodes are parsed source nodes"),
    }
}

/// The range of the first token of `node`, without its leading trivia.
pub fn first_token_range(file: P<SourceFile>, node: P<Node>) -> Range {
    let token = tsrs_scanner::get_range_of_token_at_position(
        file,
        node.pos().expect_source("lint nodes are parsed source nodes"),
    );
    Range { pos: token.source_pos().unwrap(), end: token.source_end().unwrap() }
}

pub fn replace_node(file: P<SourceFile>, node: P<Node>, text: String) -> Fix {
    Fix { text, range: trim_node_range(file, node) }
}

pub fn visit_lint_nodes(file: P<SourceFile>, visitor: &mut impl FnMut(Listener, P<Node>)) {
    fn visit(node: P<Node>, pattern: bool, visitor: &mut impl FnMut(Listener, P<Node>)) {
        let kind = node.kind();
        visitor(Listener::Enter(kind), node);
        if pattern {
            visitor(Listener::PatternEnter(kind), node);
            match kind {
                Kind::ArrayLiteralExpression => {
                    for &element in node.elements() {
                        visit(element, true, visitor);
                    }
                }
                Kind::ObjectLiteralExpression => {
                    for &property in node.properties() {
                        visit(property, true, visitor);
                    }
                }
                Kind::SpreadElement | Kind::SpreadAssignment => {
                    visit(node.expression().expect("spread expression"), true, visitor);
                }
                Kind::PropertyAssignment => {
                    visit(node.initializer().expect("property value"), true, visitor);
                }
                _ => {
                    node.for_each_child(&mut |child| {
                        visit(child, false, visitor);
                        false
                    });
                }
            }
            visitor(Listener::PatternExit(kind), node);
        } else if matches!(kind, Kind::ArrayLiteralExpression | Kind::ObjectLiteralExpression) {
            visitor(Listener::ExpressionEnter(kind), node);
            node.for_each_child(&mut |child| {
                visit(child, false, visitor);
                false
            });
            visitor(Listener::ExpressionExit(kind), node);
        } else if tsrs_ast::is_assignment_expression(node, true) {
            let expression = node.as_binary_expression();
            visit(expression.left, true, visitor);
            visit(expression.operator_token, false, visitor);
            visit(expression.right(), false, visitor);
        } else {
            node.for_each_child(&mut |child| {
                visit(child, false, visitor);
                false
            });
        }
        visitor(Listener::Exit(kind), node);
    }
    file.as_node().for_each_child(&mut |child| {
        visit(child, false, visitor);
        false
    });
}
