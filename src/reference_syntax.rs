//! Syntax evidence for two kinds of OpenAPI registration in a SCIP graph.
use proc_macro2::{LineColumn, Span};
use syn::visit::{self, Visit};
use syn::{Expr, Ident, Pat, UseTree};

use crate::git::ChangedLine;
use crate::scip::{Position, Range};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub(crate) enum ReferenceKind {
    #[default]
    Ordinary,
    Registration,
    Metadata,
    MetadataConsumer,
}

fn position(location: LineColumn) -> Position {
    Position {
        line: u32::try_from(location.line - 1).unwrap_or(u32::MAX),
        column: u32::try_from(location.column).unwrap_or(u32::MAX),
    }
}

fn range(start: Span, end: Span) -> Range {
    Range {
        start: position(start.start()),
        end: position(end.end()),
    }
}

#[derive(Default)]
pub(crate) struct ReferenceSyntax {
    spans: Vec<(Range, ReferenceKind)>,
    imports_utoipa_routes: bool,
}

impl ReferenceSyntax {
    pub(crate) fn parse(source: &str) -> Self {
        let mut syntax = Self::default();
        if let Ok(file) = syn::parse_file(source) {
            syntax.imports_utoipa_routes = file
                .items
                .iter()
                .any(|item| matches!(item, syn::Item::Use(import) if imports_routes(&import.tree)));
            syntax.visit_file(&file);
            // proc_macro2 columns count characters. SCIP's rust-analyzer
            // positions count UTF-8 bytes.
            let lines: Vec<_> = source.lines().collect();
            for (span, _) in &mut syntax.spans {
                for point in [&mut span.start, &mut span.end] {
                    if let Some(line) = lines.get(point.line as usize) {
                        point.column = line
                            .char_indices()
                            .nth(point.column as usize)
                            .map_or(line.len(), |(byte, _)| byte)
                            as u32;
                    }
                }
            }
        }
        syntax
    }

    pub(crate) fn kind(&self, at: Position) -> ReferenceKind {
        let mut registration = false;
        let mut metadata_consumer = false;
        for (_, kind) in self.spans.iter().filter(|(span, _)| span.contains(at)) {
            match kind {
                ReferenceKind::Metadata => return ReferenceKind::Metadata,
                ReferenceKind::Registration => registration = true,
                ReferenceKind::MetadataConsumer => metadata_consumer = true,
                ReferenceKind::Ordinary => {}
            }
        }
        match (registration, metadata_consumer) {
            (true, true) => ReferenceKind::Metadata,
            (true, false) => ReferenceKind::Registration,
            (false, true) => ReferenceKind::MetadataConsumer,
            (false, false) => ReferenceKind::Ordinary,
        }
    }
}

fn imports_routes(tree: &UseTree) -> bool {
    fn route_member(tree: &UseTree) -> bool {
        match tree {
            UseTree::Name(name) => name.ident == "routes",
            UseTree::Group(group) => group.items.iter().any(route_member),
            _ => false,
        }
    }
    match tree {
        UseTree::Path(path) if path.ident == "utoipa_axum" => route_member(&path.tree),
        UseTree::Group(group) => group.items.iter().any(imports_routes),
        _ => false,
    }
}

fn is_openapi_router_initializer(expr: &Expr) -> bool {
    match expr {
        Expr::MethodCall(call) => is_openapi_router_initializer(&call.receiver),
        Expr::Call(call) => {
            let Expr::Path(path) = &*call.func else {
                return false;
            };
            let mut segments = path.path.segments.iter().rev();
            segments
                .next()
                .is_some_and(|part| part.ident == "new" || part.ident == "with_openapi")
                && segments
                    .next()
                    .is_some_and(|part| part.ident == "OpenApiRouter")
        }
        _ => false,
    }
}

impl<'ast> Visit<'ast> for ReferenceSyntax {
    fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
        if attr.path().is_ident("openapi") {
            let _ = attr.parse_nested_meta(|meta| {
                if meta.input.peek(syn::token::Paren) {
                    let content;
                    let paren = syn::parenthesized!(content in meta.input);
                    content.parse::<proc_macro2::TokenStream>()?;
                    if meta.path.is_ident("paths") {
                        self.spans.push((
                            range(paren.span.open(), paren.span.close()),
                            ReferenceKind::Metadata,
                        ));
                    }
                } else if meta.input.peek(syn::Token![=]) {
                    meta.value()?.parse::<Expr>()?;
                }
                Ok(())
            });
        }
        visit::visit_attribute(self, attr);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "routes"
            && let Some(Expr::Macro(argument)) = call.args.first()
            && {
                let path: Vec<&Ident> = argument
                    .mac
                    .path
                    .segments
                    .iter()
                    .map(|part| &part.ident)
                    .collect();
                matches!(path.as_slice(), [module, name] if *module == "utoipa_axum" && *name == "routes")
                    || (self.imports_utoipa_routes && argument.mac.path.is_ident("routes"))
            }
            && argument
                .mac
                .parse_body_with(
                    syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
                )
                .is_ok()
        {
            let span = argument.mac.delimiter.span();
            self.spans.push((
                range(span.open(), span.close()),
                ReferenceKind::Registration,
            ));
        }
        visit::visit_expr_method_call(self, call);
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        use syn::spanned::Spanned;
        for (index, statement) in block.stmts.iter().enumerate() {
            let syn::Stmt::Local(local) = statement else {
                continue;
            };
            let (Pat::Ident(binding), Some(init)) = (&local.pat, &local.init) else {
                continue;
            };
            if binding.subpat.is_some() || !is_openapi_router_initializer(&init.expr) {
                continue;
            }
            struct Uses<'a> {
                name: &'a Ident,
                all: usize,
                metadata: usize,
            }
            impl<'ast> Visit<'ast> for Uses<'_> {
                fn visit_ident(&mut self, ident: &'ast Ident) {
                    self.all += usize::from(ident == self.name);
                }
                fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
                    if call.method == "get_openapi"
                        && call.args.is_empty()
                        && let Expr::Path(receiver) = &*call.receiver
                        && receiver.qself.is_none()
                        && receiver.path.is_ident(self.name)
                    {
                        self.metadata += 1;
                    }
                    visit::visit_expr_method_call(self, call);
                }
                fn visit_macro(&mut self, mac: &'ast syn::Macro) {
                    fn mentions(tokens: proc_macro2::TokenStream, name: &Ident) -> bool {
                        tokens.into_iter().any(|token| match token {
                            proc_macro2::TokenTree::Ident(ident) => ident == *name,
                            proc_macro2::TokenTree::Group(group) => mentions(group.stream(), name),
                            _ => false,
                        })
                    }
                    self.all += usize::from(mentions(mac.tokens.clone(), self.name));
                    visit::visit_macro(self, mac);
                }
            }
            let mut uses = Uses {
                name: &binding.ident,
                all: 0,
                metadata: 0,
            };
            for later in &block.stmts[index + 1..] {
                uses.visit_stmt(later);
            }
            if uses.all == 1 && uses.metadata == 1 {
                let span = init.expr.span();
                self.spans
                    .push((range(span, span), ReferenceKind::MetadataConsumer));
            }
        }
        visit::visit_block(self, block);
    }
}

// Keep signature, attributes, closing and opening brace lines conservative.
pub fn within_function_bodies(source: &str, changed: &[ChangedLine]) -> bool {
    #[derive(Default)]
    struct Bodies {
        bodies: Vec<Range>,
        nested_items: Vec<Range>,
    }

    impl Bodies {
        fn add(&mut self, block: &syn::Block) {
            use syn::spanned::Spanned;

            struct NestedItems<'a>(&'a mut Vec<Range>);
            impl<'ast> Visit<'ast> for NestedItems<'_> {
                fn visit_item(&mut self, item: &'ast syn::Item) {
                    self.0.push(range(item.span(), item.span()));
                }
            }

            let brace = block.brace_token.span;
            self.bodies.push(range(brace.open(), brace.close()));
            NestedItems(&mut self.nested_items).visit_block(block);
        }
    }
    impl<'ast> Visit<'ast> for Bodies {
        fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
            self.add(&item.block);
        }
        fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
            self.add(&item.block);
        }
    }
    let Ok(file) = syn::parse_file(source) else {
        return false;
    };
    let mut bodies = Bodies::default();
    bodies.visit_file(&file);
    !changed.is_empty()
        && changed.iter().all(|line| {
            let inside_body = bodies.bodies.iter().any(|body| match line.gap_to {
                Some(next) => body.start.line <= line.line && next <= body.end.line,
                None => body.start.line < line.line && line.line < body.end.line,
            });
            inside_body
                && !bodies.nested_items.iter().any(|item| {
                    item.start.line <= line.gap_to.unwrap_or(line.line)
                        && line.line <= item.end.line
                })
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinguishes_registration_metadata_and_calls() {
        let source = concat!(
            "#[openapi(info(title = \"test\"), paths(crate::handler))]\n",
            "struct Doc;\n",
            "fn metadata() {\n",
            "    let router = OpenApiRouter::with_openapi(api()).merge(routes_router());\n",
            "    let _ = router.get_openapi();\n",
            "}\n",
            "fn register() { router.routes(utoipa_axum::routes!(handler)); }\n",
            "fn dispatch() { handler(); }\n",
        );
        let syntax = ReferenceSyntax::parse(source);
        let kind = |line: usize, needle: &str| {
            syntax.kind(Position {
                line: line as u32,
                column: source.lines().nth(line).unwrap().find(needle).unwrap() as u32,
            })
        };
        assert_eq!(kind(0, "crate::handler"), ReferenceKind::Metadata);
        assert_eq!(kind(3, "routes_router"), ReferenceKind::MetadataConsumer);
        assert_eq!(kind(6, "handler"), ReferenceKind::Registration);
        assert_eq!(kind(7, "handler"), ReferenceKind::Ordinary);
    }

    #[test]
    fn inline_routes_read_only_as_metadata_are_metadata_edges() {
        let source = concat!(
            "fn doc() {\n",
            "    let router = OpenApiRouter::new().routes(utoipa_axum::routes!(handler));\n",
            "    let _ = router.get_openapi();\n",
            "}\n",
        );
        let syntax = ReferenceSyntax::parse(source);
        let column = source.lines().nth(1).unwrap().find("handler").unwrap() as u32;
        assert_eq!(
            syntax.kind(Position { line: 1, column }),
            ReferenceKind::Metadata
        );
    }

    #[test]
    fn local_items_do_not_make_their_attributes_or_signatures_body_edits() {
        let source = concat!(
            "fn router() {\n",
            "    #[utoipa::path(get, path = \"/test\")]\n",
            "    fn handler() {}\n",
            "    handler();\n",
            "}\n",
        );
        let line = |line| ChangedLine {
            line,
            gap_to: None,
            text: None,
            addition_only: false,
        };
        assert!(!within_function_bodies(source, &[line(1)]));
        assert!(!within_function_bodies(source, &[line(2)]));
        assert!(within_function_bodies(source, &[line(3)]));
    }

    #[test]
    fn router_used_for_dispatch_keeps_the_initializer_ordinary() {
        let source = "fn runtime() { let router = OpenApiRouter::new().merge(build()); let _ = router.get_openapi(); consume!(router); }";
        let syntax = ReferenceSyntax::parse(source);
        assert_eq!(
            syntax.kind(Position {
                line: 0,
                column: source.find("build").unwrap() as u32
            }),
            ReferenceKind::Ordinary,
        );
    }

    #[test]
    fn unrelated_routes_macro_and_non_path_arguments_stay_ordinary() {
        let source = concat!(
            "fn custom() { router.routes(other::routes!(handler)); }\n",
            "fn executes() { router.routes(utoipa_axum::routes!(call())); }\n",
        );
        let syntax = ReferenceSyntax::parse(source);
        assert!(syntax.spans.is_empty());
    }

    #[test]
    fn only_strictly_interior_lines_and_gaps_are_body_edits() {
        let source = "#[some_attr]\nfn target() {\n    let n = 1;\n    use_value(n);\n}\n";
        let line = |line, gap_to| ChangedLine {
            line,
            gap_to,
            text: None,
            addition_only: false,
        };
        assert!(within_function_bodies(
            source,
            &[line(2, None), line(2, Some(3))]
        ));
        assert!(!within_function_bodies(source, &[line(0, None)]));
        assert!(!within_function_bodies(source, &[line(1, None)]));
        assert!(within_function_bodies(source, &[line(3, Some(4))]));
        assert!(!within_function_bodies(source, &[line(4, Some(5))]));
    }
}
