use crate::config::settings::AppSettings;
use crate::downloader::template::sanitize_path_segment;
use std::path::Path;

pub struct PostMetadataExport<'a> {
    pub service: &'a str,
    pub creator_id: &'a str,
    pub creator_name: &'a str,
    pub post_id: &'a str,
    pub post_title: &'a str,
    pub published: Option<&'a str>,
    pub content: Option<&'a str>,
    pub tags: Option<&'a [String]>,
    pub origin_url: Option<String>,
    pub source_url: Option<String>,
    pub raw_json: Option<&'a str>,
}

pub fn save_post_metadata(
    target_dir: &Path,
    metadata: &PostMetadataExport<'_>,
    settings: &AppSettings,
) -> Result<(), String> {
    if !settings.download_save_metadata {
        return Ok(());
    }

    let format = settings.download_metadata_format.to_lowercase();
    let write_txt = format == "txt" || format == "both";
    let write_json = format == "json" || format == "both";

    let (txt_filename, json_filename) = if settings.download_group_by_post {
        ("info.txt".to_string(), "post.json".to_string())
    } else {
        let title = metadata.post_title.trim();
        let stem = if title.is_empty() {
            sanitize_path_segment(metadata.post_id, 80)
        } else {
            format!(
                "{} [{}]",
                sanitize_path_segment(title, 80),
                sanitize_path_segment(metadata.post_id, 40)
            )
        };
        (format!("{stem}.info.txt"), format!("{stem}.json"))
    };

    let resolved_source_url = metadata
        .source_url
        .as_deref()
        .or(metadata.origin_url.as_deref());

    if write_txt {
        let txt_path = target_dir.join(&txt_filename);
        if !txt_path.exists() {
            let mut lines = Vec::new();
            if !metadata.post_title.is_empty() {
                lines.push(format!("Title: {}", metadata.post_title));
            }
            if !metadata.creator_name.is_empty() {
                lines.push(format!(
                    "Creator: {} ({})",
                    metadata.creator_name, metadata.service
                ));
            } else {
                lines.push(format!(
                    "Creator: {} ({})",
                    metadata.creator_id, metadata.service
                ));
            }
            if let Some(pub_date) = metadata.published {
                if !pub_date.is_empty() {
                    lines.push(format!("Published: {}", pub_date));
                }
            }
            if let Some(source_url) = resolved_source_url {
                lines.push(format!("Source: {}", source_url));
            }

            if let Some(tags) = metadata.tags {
                if !tags.is_empty() {
                    lines.push(format!("Tags: {}", tags.join(", ")));
                }
            }

            if let Some(content) = metadata.content {
                let clean_text = strip_html_tags(content);
                if !clean_text.trim().is_empty() {
                    lines.push("\n--- Description ---".to_string());
                    lines.push(clean_text);
                }
            }

            let _ = std::fs::write(&txt_path, lines.join("\n"));
        }
    }

    if write_json {
        let json_path = target_dir.join(&json_filename);
        if !json_path.exists() {
            let json_str = if let Some(raw) = metadata.raw_json {
                if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(raw) {
                    if let Some(source_url) = resolved_source_url {
                        val["source_url"] = serde_json::Value::String(source_url.to_string());
                    }
                    if !metadata.creator_name.is_empty() && val.get("creator_name").is_none() {
                        val["creator_name"] =
                            serde_json::Value::String(metadata.creator_name.to_string());
                    }
                    serde_json::to_string_pretty(&val).unwrap_or_else(|_| raw.to_string())
                } else {
                    raw.to_string()
                }
            } else {
                let mut json_value = serde_json::json!({
                    "service": metadata.service,
                    "creator_id": metadata.creator_id,
                    "creator_name": metadata.creator_name,
                    "post_id": metadata.post_id,
                    "title": metadata.post_title,
                    "published": metadata.published,
                    "content": metadata.content,
                    "tags": metadata.tags,
                });
                if let Some(source_url) = resolved_source_url {
                    json_value["source_url"] = serde_json::Value::String(source_url.to_string());
                }
                serde_json::to_string_pretty(&json_value).unwrap_or_default()
            };
            let _ = std::fs::write(&json_path, json_str);
        }
    }

    Ok(())
}

fn strip_html_tags(input: &str) -> String {
    let mut output = String::new();
    let mut inside_tag = false;
    for c in input.chars() {
        if c == '<' {
            inside_tag = true;
        } else if c == '>' {
            inside_tag = false;
        } else if !inside_tag {
            output.push(c);
        }
    }
    output
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_save_post_metadata_urls() {
        let mut settings = AppSettings {
            download_save_metadata: true,
            download_metadata_format: "both".to_string(),
            download_group_by_post: true,
            ..Default::default()
        };
        settings.normalize();

        let temp_dir =
            std::env::temp_dir().join(format!("pawstash_test_meta_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).unwrap();

        let source_url = format!(
            "{}/creators/onlyfans/creator1/post/post123",
            crate::api::providers::OnlyHavenProvider::default_config().api_url
        );
        let meta_oh = PostMetadataExport {
            service: "onlyfans",
            creator_id: "creator1",
            creator_name: "Creator One",
            post_id: "post123",
            post_title: "Test Post",
            published: Some("2026-09-08"),
            content: Some("Hello <p>world</p>"),
            tags: None,
            origin_url: None,
            source_url: Some(source_url.clone()),
            raw_json: None,
        };
        save_post_metadata(&temp_dir, &meta_oh, &settings).unwrap();

        let txt = std::fs::read_to_string(temp_dir.join("info.txt")).unwrap();
        assert!(txt.contains(&format!("Source: {source_url}")));

        let json_str = std::fs::read_to_string(temp_dir.join("post.json")).unwrap();
        assert!(json_str.contains(&source_url));

        let mut flat_settings = settings.clone();
        flat_settings.download_group_by_post = false;
        save_post_metadata(&temp_dir, &meta_oh, &flat_settings).unwrap();
        assert!(temp_dir.join("Test Post [post123].info.txt").exists());
        assert!(temp_dir.join("Test Post [post123].json").exists());

        let same_title = PostMetadataExport {
            post_id: "post456",
            ..meta_oh
        };
        save_post_metadata(&temp_dir, &same_title, &flat_settings).unwrap();
        assert!(temp_dir.join("Test Post [post456].info.txt").exists());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
