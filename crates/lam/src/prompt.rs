const DEFAULT_SYSTEM_PROMPT: &str = "You are a coding agent with one tool, `eval`, which runs TypeScript in a persistent, capability-limited isolate. Ordinary JavaScript and TypeScript language primitives are available, but this is not a general Node.js or Deno runtime: imports, exports, dynamic `import()`, npm packages, and ambient filesystem, process, network, Node, or Deno APIs are unavailable. Host interaction is possible only through registered APIs injected into the namespaces listed below. Do not import modules or call unlisted platform globals. Use `lam.dir()` for complete API documentation and schemas.";

#[derive(Default)]
pub(crate) struct SystemPrompt {
    custom: Option<String>,
    annotations: Vec<String>,
}

impl SystemPrompt {
    pub(crate) fn replace(&mut self, prompt: impl Into<String>) {
        self.custom = Some(prompt.into());
    }

    pub(crate) fn annotate(&mut self, instructions: impl Into<String>) {
        self.annotations.push(instructions.into());
    }

    pub(crate) fn render(&self, api_inventory: &str) -> String {
        let generated;
        let base = match self.custom.as_deref() {
            Some(custom) => custom,
            None => {
                generated = format!("{DEFAULT_SYSTEM_PROMPT}\n\nAvailable APIs:\n{api_inventory}");
                &generated
            }
        };
        std::iter::once(base)
            .chain(self.annotations.iter().map(String::as_str))
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_includes_the_manifest_inventory() {
        let prompt = SystemPrompt::default().render("- `lam.dir(...)`");

        assert!(prompt.starts_with(DEFAULT_SYSTEM_PROMPT));
        assert!(prompt.contains("not a general Node.js or Deno runtime"));
        assert!(prompt.contains("only through registered APIs injected into the namespaces"));
        assert!(prompt.contains("Available APIs:\n- `lam.dir(...)`"));
    }

    #[test]
    fn replacement_omits_the_default_and_inventory_but_keeps_annotations() {
        let mut prompt = SystemPrompt::default();
        prompt.annotate("first");
        prompt.replace("custom");
        prompt.annotate("second");

        assert_eq!(prompt.render("inventory"), "custom\n\nfirst\n\nsecond");
    }
}
