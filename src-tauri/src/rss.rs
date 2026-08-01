use crate::{
    models::ParsedRssItem,
    network::{HttpClient, RSS_MAX_BYTES},
};
use roxmltree::{Document, Node};

pub fn fetch_and_parse(client: &HttpClient, url: &str) -> Result<Vec<ParsedRssItem>, String> {
    let bytes = client.get_bytes(url, "RSS", RSS_MAX_BYTES)?;
    let text = String::from_utf8(bytes)
        .map_err(|error| format!("RSS response is not valid UTF-8: {error}"))?;
    parse(&text)
}

pub fn parse(xml: &str) -> Result<Vec<ParsedRssItem>, String> {
    let document = Document::parse(xml).map_err(|_| "返回内容不是有效 XML".to_string())?;
    let channel = document
        .descendants()
        .find(|node| node.has_tag_name("channel"))
        .ok_or_else(|| "返回内容不是 RSS 订阅".to_string())?;

    let items = channel
        .children()
        .filter(|node| node.is_element() && node.tag_name().name() == "item")
        .filter_map(normalize_item)
        .collect();
    Ok(items)
}

#[cfg(test)]
pub fn validate_xml(xml: &str) -> Result<(), String> {
    parse(xml).and_then(validate_items).map(|_| ())
}

#[cfg(test)]
fn validate_items(items: Vec<ParsedRssItem>) -> Result<Vec<ParsedRssItem>, String> {
    if items.is_empty() {
        return Err("RSS 中没有可识别的订阅条目".to_string());
    }
    if !items.iter().any(|item| !item.enclosure_url.is_empty()) {
        return Err("RSS 中没有可下载的 torrent 条目".to_string());
    }
    Ok(items)
}

fn normalize_item(item: Node<'_, '_>) -> Option<ParsedRssItem> {
    let title = child_text(item, "title").unwrap_or_default();
    let link = child_text(item, "link").unwrap_or_default();
    let guid = child_text(item, "guid").unwrap_or_default();
    let pub_date = child_text(item, "pubDate")
        .or_else(|| descendant_text(item, "pubDate"))
        .unwrap_or_default();
    let total_bytes = descendant_text(item, "contentLength")
        .and_then(|value| value.parse().ok())
        .or_else(|| enclosure_length(item));
    let enclosure_url = extract_enclosure_url(item, &link);
    let unique_key = [&guid, &enclosure_url, &link, &title]
        .into_iter()
        .find(|value| !value.is_empty())?
        .clone();
    let title = if title.is_empty() {
        unique_key.clone()
    } else {
        title
    };
    Some(ParsedRssItem {
        title,
        link,
        guid,
        pub_date,
        total_bytes,
        enclosure_url,
        unique_key,
    })
}

fn child_text(node: Node<'_, '_>, name: &str) -> Option<String> {
    node.children()
        .find(|child| child.is_element() && child.tag_name().name() == name)
        .and_then(|child| child.text())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn descendant_text(node: Node<'_, '_>, name: &str) -> Option<String> {
    node.descendants()
        .skip(1)
        .find(|child| child.is_element() && child.tag_name().name() == name)
        .and_then(|child| child.text())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn enclosure_length(item: Node<'_, '_>) -> Option<u64> {
    item.children()
        .find(|node| node.is_element() && node.tag_name().name() == "enclosure")
        .and_then(|node| node.attribute("length"))
        .and_then(|value| value.parse().ok())
}

fn extract_enclosure_url(item: Node<'_, '_>, link: &str) -> String {
    if let Some(enclosure) = item
        .children()
        .find(|node| node.is_element() && node.tag_name().name() == "enclosure")
    {
        if let Some(url) = enclosure.attribute("url") {
            return url.trim().to_string();
        }
    }
    if let Some(torrent) = item
        .children()
        .find(|node| node.is_element() && node.tag_name().name() == "torrent")
    {
        return torrent
            .attribute("url")
            .or_else(|| torrent.text())
            .unwrap_or_default()
            .trim()
            .to_string();
    }
    let lower = link.to_ascii_lowercase();
    if lower.ends_with(".torrent") || lower.contains(".torrent?") || lower.contains(".torrent#") {
        return link.to_string();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::{parse, validate_xml};

    #[test]
    fn parses_mikan_enclosure() {
        let items = parse(
            r#"<rss><channel><item><title>示例</title><guid>g1</guid>
            <enclosure url="https://example.test/a.torrent"/></item></channel></rss>"#,
        )
        .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "示例");
        assert_eq!(items[0].unique_key, "g1");
        assert_eq!(items[0].enclosure_url, "https://example.test/a.torrent");
    }

    #[test]
    fn parses_mikan_torrent_metadata() {
        let items = parse(
            r#"<rss><channel><item><title>示例</title><guid>g1</guid>
            <torrent xmlns="https://mikanani.me/0.1/"><contentLength>938360192</contentLength>
            <pubDate>2026-07-31T01:35:21.104</pubDate></torrent>
            <enclosure length="938360192" url="https://example.test/a.torrent"/></item></channel></rss>"#,
        )
        .unwrap();
        assert_eq!(items[0].pub_date, "2026-07-31T01:35:21.104");
        assert_eq!(items[0].total_bytes, Some(938_360_192));
    }

    #[test]
    fn validates_downloadable_items() {
        assert!(validate_xml(
            r#"<rss><channel><item><title>x</title><torrent>https://e/a.torrent</torrent></item></channel></rss>"#
        )
        .is_ok());
        assert_eq!(
            validate_xml(r#"<rss><channel><item><title>x</title></item></channel></rss>"#)
                .unwrap_err(),
            "RSS 中没有可下载的 torrent 条目"
        );
    }
}
