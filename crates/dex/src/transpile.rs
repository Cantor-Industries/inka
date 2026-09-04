// Build-time TypeScript -> JavaScript transpilation for `dex build --transpile`.
// Uses the same compiler (deno_ast) that the runtime embeds, so output matches
// what runtime transpile would produce.

#[cfg(feature = "transpile")]
pub fn ts_to_js(text: &str, source_name: &str) -> Result<String, String> {
    use deno_ast::{MediaType, ParseParams, SourceMapOption, parse_module};
    use std::path::Path;

    let media_type = MediaType::from_path(Path::new(source_name));
    if !matches!(
        media_type,
        MediaType::TypeScript | MediaType::Mts | MediaType::Cts
    ) {
        return Err(format!(
            "cannot transpile '{source_name}': only plain TypeScript (.ts/.mts/.cts) is supported \
             (found media type {media_type:?})"
        ));
    }

    let specifier = deno_ast::ModuleSpecifier::parse(source_name).unwrap_or_else(|_| {
        url::Url::from_file_path(source_name)
            .unwrap_or_else(|_| url::Url::parse("file:///main.ts").unwrap())
    });

    let parsed = parse_module(ParseParams {
        specifier,
        text: text.to_string().into(),
        media_type,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    .map_err(|e| format!("failed to parse '{source_name}': {e}"))?;

    let source = parsed
        .transpile(
            &deno_ast::TranspileOptions {
                imports_not_used_as_values: deno_ast::ImportsNotUsedAsValues::Remove,
                ..Default::default()
            },
            &deno_ast::TranspileModuleOptions::default(),
            &deno_ast::EmitOptions {
                source_map: SourceMapOption::None,
                ..Default::default()
            },
        )
        .map_err(|e| format!("failed to transpile '{source_name}': {e}"))?
        .into_source();

    Ok(source.text)
}

#[cfg(not(feature = "transpile"))]
pub fn ts_to_js(_text: &str, _source_name: &str) -> Result<String, String> {
    Err("this dex build was compiled without TypeScript support (recompile with default features or enable the 'transpile' feature)".into())
}
