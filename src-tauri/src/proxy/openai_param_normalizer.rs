//! OpenAI Responses request parameter normalization.
//!
//! These rules are model-capability based, not provider based. They apply to any
//! upstream serving the same model through the OpenAI Responses API.

use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpenAiParamNormalization {
    pub applied: bool,
    pub model: Option<String>,
    pub text_verbosity_before: Option<String>,
    pub text_verbosity_after: Option<String>,
}

/// Normalize known OpenAI Responses parameter incompatibilities.
///
/// GPT-5.2-family endpoints may reject `text.verbosity` values other than
/// `medium`, while still accepting high reasoning effort such as `xhigh`.
/// Keep reasoning parameters untouched and only normalize the text verbosity
/// field when the client explicitly sent an unsupported value.
pub fn normalize_openai_response_params(body: &mut Value) -> OpenAiParamNormalization {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .map(ToString::to_string);

    let Some(model_id) = model.as_deref() else {
        return OpenAiParamNormalization::default();
    };

    if !requires_medium_text_verbosity(model_id) {
        return OpenAiParamNormalization {
            model,
            ..OpenAiParamNormalization::default()
        };
    }

    let Some(verbosity) = body.pointer_mut("/text/verbosity") else {
        return OpenAiParamNormalization {
            model,
            ..OpenAiParamNormalization::default()
        };
    };

    let Some(before) = verbosity.as_str().map(ToString::to_string) else {
        return OpenAiParamNormalization {
            model,
            ..OpenAiParamNormalization::default()
        };
    };

    if before == "medium" {
        return OpenAiParamNormalization {
            model,
            text_verbosity_before: Some(before),
            text_verbosity_after: Some("medium".to_string()),
            ..OpenAiParamNormalization::default()
        };
    }

    *verbosity = Value::String("medium".to_string());

    OpenAiParamNormalization {
        applied: true,
        model,
        text_verbosity_before: Some(before),
        text_verbosity_after: Some("medium".to_string()),
    }
}

fn requires_medium_text_verbosity(model_id: &str) -> bool {
    model_has_prefix(model_id, "gpt-5.2")
}

fn model_has_prefix(model_id: &str, prefix: &str) -> bool {
    let normalized = canonical_model_segment(model_id);
    let Some(rest) = normalized.strip_prefix(prefix) else {
        return false;
    };
    if rest.is_empty() {
        return true;
    }
    matches!(rest.as_bytes().first(), Some(b'-' | b'@' | b':' | b'_'))
}

fn canonical_model_segment(model_id: &str) -> String {
    model_id
        .trim()
        .rsplit('/')
        .next()
        .unwrap_or(model_id)
        .trim()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_gpt52_text_verbosity_without_touching_reasoning() {
        let mut body = json!({
            "model": "gpt-5.2",
            "text": { "verbosity": "low" },
            "reasoning": { "effort": "xhigh" },
            "output_config": { "effort": "xhigh" }
        });

        let result = normalize_openai_response_params(&mut body);

        assert!(result.applied);
        assert_eq!(result.model.as_deref(), Some("gpt-5.2"));
        assert_eq!(result.text_verbosity_before.as_deref(), Some("low"));
        assert_eq!(body["text"]["verbosity"], "medium");
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert_eq!(body["output_config"]["effort"], "xhigh");
    }

    #[test]
    fn normalizes_provider_prefixed_gpt52_variant() {
        let mut body = json!({
            "model": "codex-fishtrip/gpt-5.2-xhigh",
            "text": { "verbosity": "high" }
        });

        let result = normalize_openai_response_params(&mut body);

        assert!(result.applied);
        assert_eq!(body["text"]["verbosity"], "medium");
    }

    #[test]
    fn leaves_non_gpt52_text_verbosity_unchanged() {
        let mut body = json!({
            "model": "gpt-5.4",
            "text": { "verbosity": "low" }
        });

        let result = normalize_openai_response_params(&mut body);

        assert!(!result.applied);
        assert_eq!(body["text"]["verbosity"], "low");
    }

    #[test]
    fn leaves_gpt52_without_text_verbosity_unchanged() {
        let mut body = json!({
            "model": "gpt-5.2",
            "reasoning": { "effort": "xhigh" }
        });

        let result = normalize_openai_response_params(&mut body);

        assert!(!result.applied);
        assert!(body.pointer("/text/verbosity").is_none());
    }

    #[test]
    fn does_not_match_gpt520() {
        let mut body = json!({
            "model": "gpt-5.20",
            "text": { "verbosity": "low" }
        });

        let result = normalize_openai_response_params(&mut body);

        assert!(!result.applied);
        assert_eq!(body["text"]["verbosity"], "low");
    }
}
