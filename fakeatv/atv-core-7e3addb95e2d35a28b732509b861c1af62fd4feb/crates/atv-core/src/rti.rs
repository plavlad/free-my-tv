use std::io::Cursor;
use plist::{Dictionary, Value as PlistValue};

/// Generates the `_tiD` payload indicating a Remote Text Input (RTI) session is active.
pub fn build_rti_focus_payload(session_uuid: [u8; 16], current_text: &str, title: &str) -> Vec<u8> {
    let mut objects: Vec<PlistValue> = Vec::new();
    // Index 0: $null
    objects.push(PlistValue::String("$null".into()));

    // Index 1: NSUUID object
    let mut uuid_dict = Dictionary::new();
    uuid_dict.insert("NS.uuidbytes".into(), PlistValue::Data(session_uuid.to_vec()));
    uuid_dict.insert("$class".into(), PlistValue::Uid(plist::Uid::new(5)));
    objects.push(PlistValue::Dictionary(uuid_dict));

    // Index 2: documentState
    let mut doc_dict = Dictionary::new();
    doc_dict.insert("contextBeforeInput".into(), PlistValue::Uid(plist::Uid::new(3)));
    doc_dict.insert("$class".into(), PlistValue::Uid(plist::Uid::new(6)));
    objects.push(PlistValue::Dictionary(doc_dict));

    // Index 3: current text string
    objects.push(PlistValue::String(current_text.into()));

    // Index 4: documentTraits
    let mut traits_dict = Dictionary::new();
    traits_dict.insert("title".into(), PlistValue::Uid(plist::Uid::new(7)));
    traits_dict.insert("$class".into(), PlistValue::Uid(plist::Uid::new(8)));
    objects.push(PlistValue::Dictionary(traits_dict));

    // Index 5: NSUUID class
    let mut class_nsuuid = Dictionary::new();
    class_nsuuid.insert("$classname".into(), PlistValue::String("NSUUID".into()));
    class_nsuuid.insert(
        "$classes".into(),
        PlistValue::Array(vec![
            PlistValue::String("NSUUID".into()),
            PlistValue::String("NSObject".into()),
        ]),
    );
    objects.push(PlistValue::Dictionary(class_nsuuid));

    // Index 6: RTIDocumentState class
    let mut class_doc = Dictionary::new();
    class_doc.insert(
        "$classname".into(),
        PlistValue::String("RTIDocumentState".into()),
    );
    class_doc.insert(
        "$classes".into(),
        PlistValue::Array(vec![
            PlistValue::String("RTIDocumentState".into()),
            PlistValue::String("NSObject".into()),
        ]),
    );
    objects.push(PlistValue::Dictionary(class_doc));

    // Index 7: title string
    objects.push(PlistValue::String(
        if title.is_empty() {
            "Ввод текста"
        } else {
            title
        }
        .into(),
    ));

    // Index 8: RTIDocumentTraits class
    let mut class_traits = Dictionary::new();
    class_traits.insert(
        "$classname".into(),
        PlistValue::String("RTIDocumentTraits".into()),
    );
    class_traits.insert(
        "$classes".into(),
        PlistValue::Array(vec![
            PlistValue::String("RTIDocumentTraits".into()),
            PlistValue::String("NSObject".into()),
        ]),
    );
    objects.push(PlistValue::Dictionary(class_traits));

    let mut top = Dictionary::new();
    top.insert("sessionUUID".into(), PlistValue::Uid(plist::Uid::new(1)));
    top.insert("documentState".into(), PlistValue::Uid(plist::Uid::new(2)));
    top.insert("documentTraits".into(), PlistValue::Uid(plist::Uid::new(4)));

    let mut root = Dictionary::new();
    root.insert("$version".into(), PlistValue::Integer(100000.into()));
    root.insert("$archiver".into(), PlistValue::String("RTIKeyedArchiver".into()));
    root.insert("$top".into(), PlistValue::Dictionary(top));
    root.insert("$objects".into(), PlistValue::Array(objects));

    let mut buf = Vec::new();
    let _ = PlistValue::Dictionary(root).to_writer_binary(&mut buf);
    buf
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RtiTextOperation {
    /// Complete text assertion from the client (e.g. `textToAssert`, or iOS `TVRemoteUI`
    /// full-field snapshot in `keyboardOutput.insertionText` without `inputSourceState`).
    AssertText(String),
    /// Incremental delta operation with insertion and/or deletion count (sent with `inputSourceState`).
    Delta {
        deletion_count: usize,
        insertion_text: Option<String>,
    },
    /// Return / Search key pressed (`insertionText == "\n"`).
    Submit,
}

/// Parses the RTI text operation from the iPhone's `_tiC` / `_rtiData` payload.
///
/// On iOS 17–27 `TVRemoteUI`, every keystroke emits two consecutive `_tiC` frames:
/// 1. Keystroke delta (~616–729 bytes, WITH `inputSourceState`):
///    `keyboardOutput.insertionText` contains the single typed character (or `"\n"`),
///    or `deletionCount` / `producedByDeleteInput` for Backspace.
/// 2. Full-field snapshot (~431–466 bytes, WITHOUT `inputSourceState`):
///    `keyboardOutput.insertionText` (or `textToAssert`) contains the ENTIRE text box contents.
///
/// Treating frame #2 as `AssertText` rather than an incremental append eliminates
/// character/word duplication while keeping TV and iPhone text buffers strictly in sync.
pub fn parse_rti_operation(data: &[u8]) -> Option<RtiTextOperation> {
    let cursor = Cursor::new(data);
    let val = PlistValue::from_reader(cursor).ok()?;
    let dict = val.as_dictionary()?;
    let objects = dict.get("$objects")?.as_array()?;

    let resolve_string = |v: &PlistValue| -> Option<String> {
        if let Some(s) = v.as_string() {
            Some(s.to_string())
        } else if let Some(uid) = v.as_uid() {
            let idx = uid.get() as usize;
            if idx < objects.len() {
                objects[idx].as_string().map(|s| s.to_string())
            } else {
                None
            }
        } else {
            None
        }
    };

    let resolve_int = |v: &PlistValue| -> Option<usize> {
        if let Some(i) = v.as_unsigned_integer() {
            Some(i as usize)
        } else if let Some(i) = v.as_signed_integer() {
            Some(i.max(0) as usize)
        } else if let Some(uid) = v.as_uid() {
            let idx = uid.get() as usize;
            if idx < objects.len() {
                if let Some(i) = objects[idx].as_unsigned_integer() {
                    Some(i as usize)
                } else if let Some(i) = objects[idx].as_signed_integer() {
                    Some(i.max(0) as usize)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    };

    // 1. Prefer resolving directly from $top["textOperations"] or $top["root"]
    let top_dict = dict.get("$top").and_then(|v| v.as_dictionary());
    let ops_uid = top_dict.and_then(|t| {
        t.get("textOperations")
            .or_else(|| t.get("root"))
            .and_then(|v| v.as_uid())
    });

    if let Some(uid) = ops_uid {
        let ops_idx = uid.get() as usize;
        if ops_idx < objects.len() {
            if let Some(ops_dict) = objects[ops_idx].as_dictionary() {
                if let Some(tta) = ops_dict.get("textToAssert") {
                    if let Some(s) = resolve_string(tta) {
                        return Some(RtiTextOperation::AssertText(s));
                    }
                }

                let has_input_source = ops_dict.contains_key("inputSourceState");

                if let Some(kb_val) = ops_dict.get("keyboardOutput") {
                    let kb_dict = if let Some(uid) = kb_val.as_uid() {
                        let idx = uid.get() as usize;
                        if idx < objects.len() {
                            objects[idx].as_dictionary()
                        } else {
                            None
                        }
                    } else {
                        kb_val.as_dictionary()
                    };

                    if let Some(kb) = kb_dict {
                        let mut insertion_text = kb.get("insertionText").and_then(resolve_string);
                        if insertion_text.is_none() {
                            insertion_text = kb.get("textToCommit").and_then(resolve_string);
                        }

                        let mut deletion_count =
                            kb.get("deletionCount").and_then(resolve_int).unwrap_or(0);
                        if deletion_count == 0 {
                            if let Some(del_input) = kb.get("producedByDeleteInput") {
                                if del_input.as_boolean() == Some(true) {
                                    deletion_count = 1;
                                }
                            }
                        }

                        if deletion_count == 0 && insertion_text.as_deref() == Some("\n") {
                            return Some(RtiTextOperation::Submit);
                        }

                        // iOS TVRemoteUI's follow-up full-buffer sync frame has no
                        // `inputSourceState` and places the full field contents in `insertionText`.
                        if !has_input_source && deletion_count == 0 {
                            if let Some(full_text) = insertion_text {
                                return Some(RtiTextOperation::AssertText(full_text));
                            }
                        }

                        if insertion_text.is_some() || deletion_count > 0 {
                            return Some(RtiTextOperation::Delta {
                                deletion_count,
                                insertion_text,
                            });
                        }
                    }
                }
                // Known RTITextOperations object with no actionable text edit: do not fall through.
                return None;
            }
        }
    }

    // 2. Fallback: scan dictionaries in $objects (for non-standard RTI wrappers)
    let any_input_source = objects
        .iter()
        .any(|o| o.as_dictionary().map(|d| d.contains_key("inputSourceState")).unwrap_or(false));

    for obj in objects {
        if let Some(d) = obj.as_dictionary() {
            if let Some(tta) = d.get("textToAssert") {
                if let Some(s) = resolve_string(tta) {
                    return Some(RtiTextOperation::AssertText(s));
                }
            }
            if let Some(ins) = d.get("insertionText") {
                if let Some(s) = resolve_string(ins) {
                    let deletion_count = d.get("deletionCount").and_then(resolve_int).unwrap_or(0);
                    if deletion_count == 0 && s == "\n" {
                        return Some(RtiTextOperation::Submit);
                    }
                    if !any_input_source && deletion_count == 0 {
                        return Some(RtiTextOperation::AssertText(s));
                    }
                    return Some(RtiTextOperation::Delta {
                        deletion_count,
                        insertion_text: Some(s),
                    });
                }
            }
            if let Some(del) = d.get("deletionCount") {
                let deletion_count = resolve_int(del).unwrap_or(0);
                if deletion_count > 0 {
                    return Some(RtiTextOperation::Delta {
                        deletion_count,
                        insertion_text: None,
                    });
                }
            }
            if let Some(produced) = d.get("producedByDeleteInput") {
                if produced.as_boolean() == Some(true) {
                    return Some(RtiTextOperation::Delta {
                        deletion_count: 1,
                        insertion_text: None,
                    });
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use plist::Uid;

    fn make_rti_ops(
        with_input_source: bool,
        insertion: Option<&str>,
        deletion_count: Option<u64>,
        produced_by_delete: bool,
        text_to_assert: Option<&str>,
    ) -> Vec<u8> {
        let mut objects: Vec<PlistValue> = vec![PlistValue::String("$null".into())];
        let mut ops = Dictionary::new();
        ops.insert("isAutoFillTextOperation".into(), PlistValue::Boolean(false));
        ops.insert("isExplicitAutoFillInvocation".into(), PlistValue::Boolean(false));
        ops.insert("keyboardOutput".into(), PlistValue::Uid(Uid::new(2)));
        if with_input_source {
            ops.insert("inputSourceState".into(), PlistValue::Uid(Uid::new(4)));
        }
        let mut kb = Dictionary::new();
        let mut next_idx = 3u64;
        if insertion.is_some() {
            kb.insert("insertionText".into(), PlistValue::Uid(Uid::new(next_idx)));
            next_idx += 1;
        }
        if let Some(d) = deletion_count {
            kb.insert("deletionCount".into(), PlistValue::Integer(d.into()));
        }
        if produced_by_delete {
            kb.insert("producedByDeleteInput".into(), PlistValue::Boolean(true));
        }
        if text_to_assert.is_some() {
            ops.insert("textToAssert".into(), PlistValue::Uid(Uid::new(next_idx)));
        }
        objects.push(PlistValue::Dictionary(ops)); // 1
        objects.push(PlistValue::Dictionary(kb)); // 2
        if let Some(s) = insertion {
            objects.push(PlistValue::String(s.into()));
        }
        if let Some(s) = text_to_assert {
            objects.push(PlistValue::String(s.into()));
        }
        if with_input_source {
            let mut src = Dictionary::new();
            src.insert("im".into(), PlistValue::String("ru_RU@sw=Russian".into()));
            objects.push(PlistValue::Dictionary(src));
        }

        let mut top = Dictionary::new();
        top.insert("textOperations".into(), PlistValue::Uid(Uid::new(1)));
        let mut root = Dictionary::new();
        root.insert("$version".into(), PlistValue::Integer(100000.into()));
        root.insert("$archiver".into(), PlistValue::String("RTIKeyedArchiver".into()));
        root.insert("$top".into(), PlistValue::Dictionary(top));
        root.insert("$objects".into(), PlistValue::Array(objects));

        let mut buf = Vec::new();
        PlistValue::Dictionary(root).to_writer_binary(&mut buf).unwrap();
        buf
    }

    #[test]
    fn ios_keystroke_and_snapshot_pair_do_not_duplicate() {
        let delta_pkt = make_rti_ops(true, Some("р"), None, false, None);
        let snap_pkt = make_rti_ops(false, Some("Пр"), None, false, None);

        assert_eq!(
            parse_rti_operation(&delta_pkt),
            Some(RtiTextOperation::Delta {
                deletion_count: 0,
                insertion_text: Some("р".into()),
            })
        );
        assert_eq!(
            parse_rti_operation(&snap_pkt),
            Some(RtiTextOperation::AssertText("Пр".into()))
        );
    }

    #[test]
    fn ios_backspace_clear_and_return() {
        let bs = make_rti_ops(true, None, Some(1), true, None);
        let clear = make_rti_ops(true, None, None, false, Some(""));
        let ret = make_rti_ops(true, Some("\n"), None, false, None);

        assert_eq!(
            parse_rti_operation(&bs),
            Some(RtiTextOperation::Delta {
                deletion_count: 1,
                insertion_text: None,
            })
        );
        assert_eq!(
            parse_rti_operation(&clear),
            Some(RtiTextOperation::AssertText(String::new()))
        );
        assert_eq!(parse_rti_operation(&ret), Some(RtiTextOperation::Submit));
    }
}
