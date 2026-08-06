use deno_ast::swc::ast as swc_ast;
use deno_ast::swc::ecma_visit::{Visit, VisitWith, noop_visit_type};
use deno_ast::{
    EmitOptions, ImportsNotUsedAsValues, MediaType, ModuleKind, ParseParams, SourceMapOption,
    TranspileModuleOptions, TranspileOptions,
};
use deno_core::{ModuleCodeString, ModuleName, SourceMapData};
use deno_error::JsErrorBox;

use crate::error::EvalError;

pub(crate) const RUNTIME_ESM_SPECIFIER: &str = "ext:lam_runtime/runtime.ts";

pub(crate) fn transpile(source: &str, cell_id: u64) -> Result<String, EvalError> {
    let specifier = deno_ast::ModuleSpecifier::parse(&format!("file:///lam/cell-{cell_id}.ts"))
        .map_err(EvalError::internal)?;
    let source_url = format!("lam://cell/{cell_id}");
    transpile_source(source, specifier, "'use strict'; void 0;", &source_url).map_err(|message| {
        EvalError::Transpile {
            message: annotate_transpile_message(source, message),
        }
    })
}

/// When transpile fails and the source contains backticks, remind the model of
/// the usual cause: an unescaped backtick inside a template literal.
fn annotate_transpile_message(source: &str, message: String) -> String {
    if source.contains('`')
        && !message.contains("template literal")
        && !message.contains("backtick")
    {
        format!(
            "{message}\nnote: a bare backtick inside a template literal is invalid TypeScript; escape it as \\` or, for multi-line text that must contain backticks, use double-quoted strings or a string[] of lines (lam.edit.apply patch / lam.edit.write content accept string | string[])."
        )
    } else {
        message
    }
}

pub(crate) fn extension(
    specifier: ModuleName,
    source: ModuleCodeString,
) -> Result<(ModuleCodeString, Option<SourceMapData>), JsErrorBox> {
    if specifier.as_str() != RUNTIME_ESM_SPECIFIER {
        return Ok((source, None));
    }

    let specifier = deno_ast::ModuleSpecifier::parse(&specifier)
        .map_err(|error| JsErrorBox::generic(error.to_string()))?;
    let (source, source_map) =
        transpile_module(source.as_str(), specifier, false, SourceMapOption::Separate)
            .map_err(JsErrorBox::generic)?;

    Ok((
        source.into(),
        source_map.map(|source_map| source_map.into_bytes().into()),
    ))
}

fn transpile_source(
    source: &str,
    specifier: deno_ast::ModuleSpecifier,
    prologue: &str,
    source_url: &str,
) -> Result<String, String> {
    let (emitted, _) = transpile_module(source, specifier, true, SourceMapOption::None)?;

    Ok(format!("{prologue}\n{emitted}\n//# sourceURL={source_url}"))
}

fn transpile_module(
    source: &str,
    specifier: deno_ast::ModuleSpecifier,
    reject_module_syntax: bool,
    source_map: SourceMapOption,
) -> Result<(String, Option<String>), String> {
    let parsed = deno_ast::parse_module(ParseParams {
        specifier,
        text: source.to_owned().into(),
        media_type: MediaType::TypeScript,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    .map_err(|error| error.to_string())?;

    if reject_module_syntax {
        let mut module_syntax = ModuleSyntaxDetector::default();
        parsed.program().visit_with(&mut module_syntax);
        if module_syntax.found {
            return Err(
                "imports, exports, and dynamic import() are not supported in this eval kernel"
                    .to_owned(),
            );
        }
    }

    let emitted = parsed
        .transpile(
            &TranspileOptions {
                imports_not_used_as_values: ImportsNotUsedAsValues::Remove,
                var_decl_imports: false,
                ..Default::default()
            },
            &TranspileModuleOptions {
                module_kind: Some(ModuleKind::Esm),
            },
            &EmitOptions {
                source_map,
                inline_sources: true,
                remove_comments: false,
                ..Default::default()
            },
        )
        .map_err(|error| error.to_string())?
        .into_source();

    Ok((emitted.text, emitted.source_map))
}

#[derive(Default)]
struct ModuleSyntaxDetector {
    found: bool,
}

impl Visit for ModuleSyntaxDetector {
    noop_visit_type!();

    fn visit_call_expr(&mut self, expression: &swc_ast::CallExpr) {
        if matches!(expression.callee, swc_ast::Callee::Import(_)) {
            self.found = true;
        }
        expression.visit_children_with(self);
    }

    fn visit_module_decl(&mut self, declaration: &swc_ast::ModuleDecl) {
        self.found = true;
        declaration.visit_children_with(self);
    }
}
