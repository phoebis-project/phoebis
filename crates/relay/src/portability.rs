//! Whether a request body can move between accounts: a reference to account-scoped state binds it to the
//! account that holds that state.

use phoebis_routing::Portability;
use serde_json::Value;

/// The class of the parsed body's references to account state: the hold class for a top-level `container`, else the
/// strongest class that a block of `messages` gives. The relay's `BodyFacts::portability` reads it off the parse it
/// already ran.
pub fn portable_of(root: &Value) -> Portability {
    let Some(object) = root.as_object() else {
        return Portability::Portable;
    };
    if object.contains_key("container") {
        return Portability::Hold;
    }
    object
        .get("messages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|message| message.get("content").and_then(Value::as_array))
        .flatten()
        .map(block_class)
        .max()
        .unwrap_or(Portability::Portable)
}

/// The class a block gives the request, with the blocks below it: the release class for a `web_search_tool_result`,
/// the hold class for another type that ends in `_tool_result` (a `web_fetch_tool_result` aside) or for a `file_id`
/// on the block itself or in its `source`, and the portable class otherwise. The walk then reads the blocks of the
/// block's own `content` array and of a `source` whose own `content` is an array, which is a document's content
/// source: the positions the Messages API places blocks at, and nothing else, so no `input` of a tool call, no tool
/// definition, no string and no `content` that is an object. The strongest class stands, so a web search result
/// beside a hold reference never lowers it.
fn block_class(block: &Value) -> Portability {
    let by_type = match block.get("type").and_then(Value::as_str) {
        Some("web_search_tool_result") => Portability::Release,
        // A web fetch result holds no encrypted member, and a second account took one as it was sent.
        Some("web_fetch_tool_result") => Portability::Portable,
        Some(kind) if kind.ends_with("_tool_result") => Portability::Hold,
        _ => Portability::Portable,
    };
    let source = block.get("source");
    let by_file = if block.get("file_id").is_some()
        || source.and_then(|source| source.get("file_id")).is_some()
    {
        Portability::Hold
    } else {
        Portability::Portable
    };
    let own_blocks = block.get("content").and_then(Value::as_array);
    let source_blocks = source
        .and_then(|source| source.get("content"))
        .and_then(Value::as_array);
    own_blocks
        .into_iter()
        .chain(source_blocks)
        .flatten()
        .map(block_class)
        .fold(by_type.max(by_file), Portability::max)
}

#[cfg(test)]
mod tests {
    use super::{portable_of, Portability};

    /// The body as the relay's facts read it: through the lenient parse, a body no reader parses read as portable.
    fn class_of(body: &[u8]) -> Portability {
        phoebis_guardrails::parse_body(body)
            .map_or(Portability::Portable, |root| portable_of(&root))
    }

    #[test]
    fn account_scoped_references_make_a_request_non_portable() {
        assert_eq!(
            class_of(br#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#),
            Portability::Portable
        );
        assert_eq!(
            class_of(br#"{"model":"m","container":"cont_1","messages":[]}"#),
            Portability::Hold
        );
        let document = br#"{"model":"m","messages":[{"role":"user","content":[{"type":"document","source":{"type":"file","file_id":"file_1"}}]}]}"#;
        assert_eq!(class_of(document), Portability::Hold);
        let server_tool = br#"{"model":"m","messages":[{"role":"user","content":[{"type":"code_execution_tool_result","tool_use_id":"x","content":{}}]}]}"#;
        assert_eq!(class_of(server_tool), Portability::Hold);
        let client_tool = br#"{"model":"m","messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"x","content":"ok"}]}]}"#;
        assert_eq!(
            class_of(client_tool),
            Portability::Portable,
            "a client tool result is portable"
        );
        let text =
            br#"{"model":"m","messages":[{"role":"user","content":"the word file_id in text"}]}"#;
        assert_eq!(
            class_of(text),
            Portability::Portable,
            "a string mentioning the key is not a reference"
        );
    }

    #[test]
    fn a_body_cut_mid_pair_is_read_for_its_references() {
        // A lone surrogate escape, which the client sends for a string cut mid-pair: a strict decoder refuses the
        // body, and the container it names binds it all the same.
        let cut = br#"{"model":"m","container":"cont_1","messages":[{"role":"user","content":"cut \ud83d"}]}"#;
        assert!(serde_json::from_slice::<serde_json::Value>(cut).is_err());
        assert_eq!(class_of(cut), Portability::Hold);
        assert_eq!(
            class_of(br#"{"model":"m","messages":[{"role":"user","content":"cut \ud83d"}]}"#),
            Portability::Portable
        );
    }

    #[test]
    fn a_tool_calls_input_binds_no_request() {
        let container = br#"{"model":"m","messages":[{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"batch","input":{"container":{"kind":"project","id":"p"}}}]}]}"#;
        assert_eq!(class_of(container), Portability::Portable);
        let file = br#"{"model":"m","messages":[{"role":"assistant","content":[{"type":"tool_use","id":"toolu_1","name":"batch","input":{"file_id":"file_1"}}]}]}"#;
        assert_eq!(class_of(file), Portability::Portable);
    }

    #[test]
    fn a_tool_definition_binds_no_request() {
        let body = br#"{"model":"m","messages":[{"role":"system","content":[{"type":"tool_addition","tool":{"definition":{"name":"update","input_schema":{"type":"object","properties":{"file_id":{"type":"string"}}}}}}]}]}"#;
        assert_eq!(class_of(body), Portability::Portable);
    }

    #[test]
    fn a_file_inside_a_tool_results_content_binds_the_request() {
        let body = br#"{"model":"m","messages":[{"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":[{"type":"image","source":{"type":"file","file_id":"file_1"}}]}]}]}"#;
        assert_eq!(class_of(body), Portability::Hold);
    }

    #[test]
    fn a_container_upload_binds_the_request() {
        let body = br#"{"model":"m","messages":[{"role":"user","content":[{"type":"container_upload","file_id":"file_1"}]}]}"#;
        assert_eq!(class_of(body), Portability::Hold);
    }

    #[test]
    fn a_file_inside_a_documents_content_source_binds_the_request() {
        let body = br#"{"model":"m","messages":[{"role":"user","content":[{"type":"document","source":{"type":"content","content":[{"type":"image","source":{"type":"file","file_id":"file_1"}}]}}]}]}"#;
        assert_eq!(class_of(body), Portability::Hold);
    }

    #[test]
    fn a_web_search_result_alone_gives_the_release_class_and_a_file_beside_it_the_hold_class() {
        let alone = br#"{"model":"m","messages":[{"role":"assistant","content":[{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[{"type":"web_search_result","url":"https://example.com","title":"t","encrypted_content":"e"}]}]}]}"#;
        assert_eq!(class_of(alone), Portability::Release);
        let file_behind = br#"{"model":"m","messages":[{"role":"assistant","content":[{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[{"type":"web_search_result","url":"https://example.com","title":"t","encrypted_content":"e"}]}]},{"role":"user","content":[{"type":"document","source":{"type":"file","file_id":"file_1"}}]}]}"#;
        assert_eq!(class_of(file_behind), Portability::Hold);
        let file_ahead = br#"{"model":"m","messages":[{"role":"user","content":[{"type":"document","source":{"type":"file","file_id":"file_1"}}]},{"role":"assistant","content":[{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[{"type":"web_search_result","url":"https://example.com","title":"t","encrypted_content":"e"}]}]}]}"#;
        assert_eq!(class_of(file_ahead), Portability::Hold);
    }

    #[test]
    fn a_web_fetch_result_binds_no_request() {
        let body = br#"{"model":"m","messages":[{"role":"assistant","content":[{"type":"web_fetch_tool_result","tool_use_id":"srvtoolu_1","content":{"type":"web_fetch_result","url":"https://example.com","content":{"type":"document","source":{"type":"text","media_type":"text/plain","data":"hi"}}}}]}]}"#;
        assert_eq!(class_of(body), Portability::Portable);
    }
}
