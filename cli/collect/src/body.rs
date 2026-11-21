//! An answer's body as a tree, so it can be read (for paging and planning), redacted, and written back in the same
//! format. AWS answers are XML (EC2 and the query protocols, S3) or JSON (everything else).

use anyhow::{Context, Result};
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::{Reader, Writer, XmlVersion};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    Empty,
    Xml(Element),
    Json(Value),
    /// Anything else, kept as is and never redacted structurally.
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
    pub text: String,
}

impl Element {
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    /// Every element reached by following `path` from here, through every match at each step.
    pub fn find_all<'a>(&'a self, path: &[&str]) -> Vec<&'a Element> {
        let mut current = vec![self];
        for step in path {
            current = current
                .iter()
                .flat_map(|e| e.children.iter().filter(|c| c.name == *step))
                .collect();
        }
        current
    }

    pub fn text_of(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.as_str())
    }
}

impl Body {
    pub fn parse(bytes: &[u8]) -> Body {
        let text = String::from_utf8_lossy(bytes);
        let trimmed = text.trim_start();
        if trimmed.is_empty() {
            Body::Empty
        } else if trimmed.starts_with('<') {
            parse_xml(trimmed)
                .map(Body::Xml)
                .unwrap_or_else(|_| Body::Text(text.into_owned()))
        } else if trimmed.starts_with('{') || trimmed.starts_with('[') {
            serde_json::from_str(trimmed)
                .map(Body::Json)
                .unwrap_or_else(|_| Body::Text(text.into_owned()))
        } else {
            Body::Text(text.into_owned())
        }
    }

    pub fn serialize(&self) -> String {
        match self {
            Body::Empty => String::new(),
            Body::Xml(root) => write_xml(root),
            Body::Json(v) => v.to_string(),
            Body::Text(t) => t.clone(),
        }
    }

    /// A string field at the top of the answer, or one or two levels down (the query protocols wrap answers in
    /// `<XResponse><XResult>`). Used for paging tokens.
    pub fn find_shallow(&self, name: &str) -> Option<String> {
        match self {
            Body::Xml(root) => {
                let direct = root.child(name);
                let nested = || root.children.iter().find_map(|c| c.child(name));
                direct.or_else(nested).map(|e| e.text.clone())
            }
            Body::Json(Value::Object(map)) => match map.get(name) {
                Some(Value::String(s)) => Some(s.clone()),
                _ => map
                    .values()
                    .find_map(|v| v.get(name).and_then(Value::as_str).map(str::to_string)),
            },
            _ => None,
        }
    }

    pub fn xml(&self) -> Option<&Element> {
        match self {
            Body::Xml(e) => Some(e),
            _ => None,
        }
    }

    pub fn json(&self) -> Option<&Value> {
        match self {
            Body::Json(v) => Some(v),
            _ => None,
        }
    }
}

fn parse_xml(text: &str) -> Result<Element> {
    let mut reader = Reader::from_str(text);
    let mut stack: Vec<Element> = Vec::new();
    let mut root = None;
    loop {
        match reader.read_event().context("reading XML")? {
            Event::Start(start) => stack.push(element_from(&start)?),
            Event::Empty(start) => {
                let element = element_from(&start)?;
                close(&mut stack, &mut root, element);
            }
            Event::End(_) => {
                let element = stack.pop().context("unbalanced XML")?;
                close(&mut stack, &mut root, element);
            }
            Event::Text(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&t.xml_content(XmlVersion::default()));
                }
            }
            Event::GeneralRef(r) => {
                if let Some(top) = stack.last_mut() {
                    let name: &str = &r;
                    let resolved = match name {
                        "amp" => "&".to_string(),
                        "lt" => "<".to_string(),
                        "gt" => ">".to_string(),
                        "quot" => "\"".to_string(),
                        "apos" => "'".to_string(),
                        other => match r.resolve_char_ref().ok().flatten() {
                            Some(c) => c.to_string(),
                            None => format!("&{other};"),
                        },
                    };
                    top.text.push_str(&resolved);
                }
            }
            Event::CData(c) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&c.into_inner());
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    let mut root = root.context("no XML root element")?;
    trim_layout(&mut root);
    Ok(root)
}

fn element_from(start: &BytesStart) -> Result<Element> {
    let name = start.name().as_ref().to_string();
    let mut attrs = Vec::new();
    for attr in start.attributes() {
        let attr = attr.context("XML attribute")?;
        attrs.push((
            attr.key.as_ref().to_string(),
            attr.normalized_value(XmlVersion::default())
                .context("XML attribute value")?
                .into_owned(),
        ));
    }
    Ok(Element {
        name,
        attrs,
        children: Vec::new(),
        text: String::new(),
    })
}

fn close(stack: &mut [Element], root: &mut Option<Element>, element: Element) {
    match stack.last_mut() {
        Some(parent) => parent.children.push(element),
        None => *root = Some(element),
    }
}

/// Whitespace between child elements is layout, not data.
fn trim_layout(element: &mut Element) {
    if !element.children.is_empty() && element.text.trim().is_empty() {
        element.text.clear();
    }
    for child in &mut element.children {
        trim_layout(child);
    }
}

fn write_xml(root: &Element) -> String {
    let mut writer = Writer::new(Vec::new());
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .expect("writing to a Vec");
    write_element(&mut writer, root);
    String::from_utf8(writer.into_inner()).expect("XML written from strings is UTF-8")
}

fn write_element(writer: &mut Writer<Vec<u8>>, element: &Element) {
    let mut start = BytesStart::new(element.name.as_str());
    for (k, v) in &element.attrs {
        start.push_attribute((k.as_str(), v.as_str()));
    }
    writer.write_event(Event::Start(start)).expect("writing to a Vec");
    if !element.text.is_empty() {
        writer
            .write_event(Event::Text(BytesText::new(&element.text)))
            .expect("writing to a Vec");
    }
    for child in &element.children {
        write_element(writer, child);
    }
    writer
        .write_event(Event::End(BytesEnd::new(element.name.as_str())))
        .expect("writing to a Vec");
}

#[cfg(test)]
mod tests {
    use super::*;

    const EC2: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<DescribeInstancesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
    <requestId>r-1</requestId>
    <reservationSet>
        <item>
            <instancesSet>
                <item>
                    <instanceType>m5.large</instanceType>
                    <tagSet>
                        <item><key>Name</key><value>web &amp; api</value></item>
                    </tagSet>
                </item>
            </instancesSet>
        </item>
    </reservationSet>
    <nextToken>abc</nextToken>
</DescribeInstancesResponse>"#;

    #[test]
    fn xml_round_trips_and_reads() {
        let body = Body::parse(EC2.as_bytes());
        assert_eq!(body.find_shallow("nextToken").as_deref(), Some("abc"));
        let root = body.xml().unwrap();
        let instances = root.find_all(&["reservationSet", "item", "instancesSet", "item"]);
        assert_eq!(instances[0].text_of("instanceType"), Some("m5.large"));
        let again = Body::parse(body.serialize().as_bytes());
        assert_eq!(again, body);
        assert!(body.serialize().contains("web &amp; api"));
    }

    #[test]
    fn query_protocol_token_is_one_level_down() {
        let xml = "<DescribeLoadBalancersResponse><DescribeLoadBalancersResult><NextMarker>m2</NextMarker>\
                   </DescribeLoadBalancersResult></DescribeLoadBalancersResponse>";
        assert_eq!(
            Body::parse(xml.as_bytes()).find_shallow("NextMarker").as_deref(),
            Some("m2")
        );
    }

    #[test]
    fn json_and_empty() {
        assert_eq!(
            Body::parse(br#"{"NextToken":"t"}"#)
                .find_shallow("NextToken")
                .as_deref(),
            Some("t")
        );
        assert_eq!(Body::parse(b"  "), Body::Empty);
    }
}
