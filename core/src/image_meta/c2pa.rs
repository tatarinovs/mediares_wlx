//! Reads the active embedded manifest's source declaration for display.

use ciborium::Value;

/// The JUMBF entity, fragment sequence and repeated box contents of a JPEG XT APP11.
pub(crate) fn jpeg_fragment(segment: &[u8]) -> Option<([u8; 2], u32, &[u8])> {
    let (prefix, part) = segment.split_at_checked(8)?;
    if &prefix[..2] != b"JP" {
        return None;
    }
    let entity = [prefix[2], prefix[3]];
    let sequence = u32::from_be_bytes(prefix[4..8].try_into().ok()?);
    Some((entity, sequence, part))
}

pub(crate) fn jpeg_manifest_start(part: &[u8]) -> bool {
    if part.get(4..8) != Some(b"jumb") {
        return false;
    }
    let header_len = if part.starts_with(&1u32.to_be_bytes()) {
        16
    } else {
        8
    };
    part.get(header_len..)
        .and_then(|mut body| description(&mut body, b"c2pa"))
        == Some(b"c2pa")
}

#[derive(Default)]
pub(super) struct JpegManifest {
    data: Vec<u8>,
    entity: [u8; 2],
    sequence: u32,
    invalid: bool,
}

impl JpegManifest {
    pub(super) fn push(&mut self, segment: &[u8]) {
        if self.invalid {
            return;
        }
        let Some((entity, sequence, part)) = jpeg_fragment(segment) else {
            return;
        };
        if self.data.is_empty() {
            if sequence == 1 && jpeg_manifest_start(part) {
                self.entity = entity;
                self.sequence = sequence;
                self.data.extend_from_slice(part);
            }
            return;
        }
        if entity != self.entity {
            return;
        }
        // APP11 repeats the top-level JUMBF box header at the start of each fragment.
        let header_len = if self.data.starts_with(&1u32.to_be_bytes()) {
            16
        } else {
            8
        };
        if sequence != self.sequence + 1 || part.get(..header_len) != self.data.get(..header_len) {
            self.invalid = true;
            return;
        }
        self.sequence = sequence;
        self.data.extend_from_slice(&part[header_len..]);
    }

    pub(super) fn ai_source(self) -> Option<String> {
        if self.invalid {
            return None;
        }
        ai_source(&self.data)
    }
}

struct JumbfBox<'a> {
    kind: &'a [u8],
    body: &'a [u8],
}

fn next_box<'a>(bytes: &mut &'a [u8]) -> Option<JumbfBox<'a>> {
    let (header, _) = bytes.split_at_checked(8)?;
    let kind = &header[4..8];
    let short_len = u32::from_be_bytes(header[..4].try_into().ok()?);
    let (len, header_len) = match short_len {
        0 => (bytes.len(), 8),
        1 => (
            usize::try_from(u64::from_be_bytes(bytes.get(8..16)?.try_into().ok()?)).ok()?,
            16,
        ),
        n => (n as usize, 8),
    };
    let (full, rest) = bytes.split_at_checked(len)?;
    let body = full.get(header_len..)?;
    *bytes = rest;
    Some(JumbfBox { kind, body })
}

fn description<'a>(body: &mut &'a [u8], uuid_prefix: &[u8; 4]) -> Option<&'a [u8]> {
    let desc = next_box(body)?;
    const UUID_SUFFIX: &[u8] = b"\x00\x11\x00\x10\x80\x00\x00\xaa\x00\x38\x9b\x71";
    if desc.kind != b"jumd"
        || !desc.body.starts_with(uuid_prefix)
        || desc.body.get(4..16)? != UUID_SUFFIX
        || desc.body.get(16)? & 2 == 0
    {
        return None;
    }
    let label = desc.body.get(17..)?;
    label.get(..label.iter().position(|&b| b == 0)?)
}

pub(super) fn ai_source(mut bytes: &[u8]) -> Option<String> {
    let store = next_box(&mut bytes)?;
    if store.kind != b"jumb" || !bytes.is_empty() {
        return None;
    }
    let mut children = store.body;
    if description(&mut children, b"c2pa")? != b"c2pa" {
        return None;
    }
    let mut active = None;
    while !children.is_empty() {
        let entry = next_box(&mut children)?;
        let mut body = entry.body;
        if entry.kind == b"jumb"
            && [b"c2ma", b"c2md", b"c2cm", b"c2um"]
                .into_iter()
                .any(|kind| {
                    body = entry.body;
                    description(&mut body, kind)
                        .is_some_and(|label| label.starts_with(b"urn:c2pa:"))
                })
        {
            active = Some(body);
        }
    }
    let mut manifest = active?;
    while !manifest.is_empty() {
        let entry = next_box(&mut manifest)?;
        let mut assertions = entry.body;
        if entry.kind == b"jumb"
            && description(&mut assertions, b"c2as") == Some(b"c2pa.assertions")
        {
            return action_source(assertions);
        }
    }
    None
}

fn action_source(mut assertions: &[u8]) -> Option<String> {
    let mut fallback = None;
    while !assertions.is_empty() {
        let entry = next_box(&mut assertions)?;
        let mut body = entry.body;
        if entry.kind != b"jumb"
            || !matches!(
                description(&mut body, b"cbor"),
                Some(b"c2pa.actions" | b"c2pa.actions.v2")
            )
        {
            continue;
        }
        let data = next_box(&mut body)?;
        if data.kind != b"cbor" {
            continue;
        }
        let Ok(value) = ciborium::de::from_reader_with_recursion_limit::<Value, _>(data.body, 24)
        else {
            continue;
        };
        for action in member(&value, "actions")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if let Some(summary) = describe_action(action) {
                if member(action, "action").and_then(Value::as_text) == Some("c2pa.created") {
                    return Some(summary);
                }
                fallback.get_or_insert(summary);
            }
        }
    }
    fallback
}

fn member<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value
        .as_map()?
        .iter()
        .find_map(|(k, v)| (k.as_text()? == key).then_some(v))
}

fn describe_action(action: &Value) -> Option<String> {
    let uri = member(action, "digitalSourceType")?.as_text()?;
    let source_type = uri
        .strip_prefix("http://")
        .or_else(|| uri.strip_prefix("https://"))?
        .strip_prefix("cv.iptc.org/newscodes/digitalsourcetype/")?;
    let category = match source_type {
        "trainedAlgorithmicMedia" => "AI-generated",
        "compositeWithTrainedAlgorithmicMedia" => "AI composite",
        _ => return None,
    };
    let agent = member(action, "softwareAgent").and_then(|agent| {
        let name = agent
            .as_text()
            .or_else(|| member(agent, "name")?.as_text())?;
        let name = super::normalize_ai_text(name)?;
        match member(agent, "version")
            .and_then(Value::as_text)
            .and_then(super::normalize_ai_text)
        {
            Some(version) => Some(format!("{name} / {version}")),
            None => Some(name),
        }
    });
    let detail =
        agent.or_else(|| super::normalize_ai_text(member(action, "description")?.as_text()?));
    let summary = match detail {
        Some(detail) => format!("C2PA: {category} — {detail}"),
        None => format!("C2PA: {category}"),
    };
    super::normalize_ai_text(&summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut box_bytes = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        box_bytes.extend_from_slice(kind);
        box_bytes.extend_from_slice(body);
        box_bytes
    }

    fn superbox(uuid_prefix: &[u8; 4], label: &str, contents: &[u8]) -> Vec<u8> {
        let mut desc = uuid_prefix.to_vec();
        desc.extend_from_slice(b"\x00\x11\x00\x10\x80\x00\x00\xaa\x00\x38\x9b\x71\x03");
        desc.extend_from_slice(label.as_bytes());
        desc.push(0);
        let mut body = wrap(b"jumd", &desc);
        body.extend_from_slice(contents);
        wrap(b"jumb", &body)
    }

    fn manifest(source: &str) -> Vec<u8> {
        let action = Value::Map(vec![
            ("action".into(), "c2pa.created".into()),
            (
                "digitalSourceType".into(),
                format!("http://cv.iptc.org/newscodes/digitalsourcetype/{source}").into(),
            ),
            (
                "softwareAgent".into(),
                Value::Map(vec![
                    ("name".into(), "ChatGPT".into()),
                    ("version".into(), "gpt-image".into()),
                ]),
            ),
        ]);
        let mut cbor = Vec::new();
        ciborium::into_writer(
            &Value::Map(vec![("actions".into(), Value::Array(vec![action]))]),
            &mut cbor,
        )
        .unwrap();
        let action_box = superbox(b"cbor", "c2pa.actions.v2", &wrap(b"cbor", &cbor));
        let assertions = superbox(b"c2as", "c2pa.assertions", &action_box);
        superbox(b"c2ma", "urn:c2pa:test", &assertions)
    }

    #[test]
    fn reads_only_the_active_manifest_and_reassembles_jpeg_segments() {
        let ai = manifest("trainedAlgorithmicMedia");
        let expected = Some("C2PA: AI-generated — ChatGPT / gpt-image");
        let store = superbox(b"c2pa", "c2pa", &ai);
        assert_eq!(ai_source(&store).as_deref(), expected);
        let mut both = ai;
        both.extend(manifest("digitalCapture"));
        assert_eq!(ai_source(&superbox(b"c2pa", "c2pa", &both)), None);

        let mut jpeg = JpegManifest::default();
        let cut = 72;
        let mut first = b"JP\0\x01\0\0\0\x01".to_vec();
        first.extend_from_slice(&store[..cut]);
        let mut second = b"JP\0\x01\0\0\0\x02".to_vec();
        second.extend_from_slice(&store[..8]);
        second.extend_from_slice(&store[cut..]);
        jpeg.push(&first);
        jpeg.push(&second);
        assert_eq!(jpeg.ai_source().as_deref(), expected);

        let app11 = |payload: &[u8]| {
            let mut segment = vec![0xFF, 0xEB];
            segment.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
            segment.extend_from_slice(payload);
            segment
        };
        let other = app11(b"JP\0\x02\0\0\0\x01unrelated APP11 data");
        let mut sample = image::DynamicImage::new_rgb8(8, 8);
        *sample.as_mut_rgb8().unwrap().get_pixel_mut(0, 0) = image::Rgb([108, 45, 0]);
        let mut bytes = std::io::Cursor::new(Vec::new());
        sample
            .write_to(&mut bytes, image::ImageFormat::Jpeg)
            .unwrap();
        let mut bytes = bytes.into_inner();
        let expected_pixels = image::load_from_memory(&bytes).unwrap();
        bytes.splice(
            2..2,
            [app11(&first), other.clone(), app11(&second)].concat(),
        );
        let cleaned = crate::jpeg::with_only_exif(&bytes, b"II*\0").unwrap();
        assert!(!cleaned.windows(first.len()).any(|data| data == first));
        assert!(!cleaned.windows(second.len()).any(|data| data == second));
        assert!(cleaned.windows(other.len()).any(|data| data == other));
        assert_eq!(image::load_from_memory(&cleaned).unwrap(), expected_pixels);

        let mut broken = JpegManifest::default();
        second[7] = 3;
        broken.push(&first);
        broken.push(&second);
        assert_eq!(broken.ai_source(), None);
    }
}
