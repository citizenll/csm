use anyhow::Context;
use anyhow::Result;
use codex_core::config::CONFIG_TOML_FILE;
use codex_core::config::ConfigToml;
use codex_core::config::edit::ConfigEdit;
use std::path::Path;

const GENERATED_PROFILE_PREFIXES: [&str; 1] = ["smart-"];

pub(crate) async fn cleanup_generated_profiles(
    codex_home: &Path,
    preserve_profiles: &[Option<&str>],
) -> Result<Vec<String>> {
    let config_path = codex_home.join(CONFIG_TOML_FILE);
    if !config_path.exists() {
        return Ok(Vec::new());
    }

    let contents = std::fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let config = toml::from_str::<ConfigToml>(&contents)
        .with_context(|| format!("failed to parse {}", config_path.display()))?;

    let preserve = preserve_profiles
        .iter()
        .flatten()
        .map(|profile| profile.to_string())
        .collect::<std::collections::HashSet<_>>();

    let mut removed_profiles = config
        .profiles
        .keys()
        .filter(|name| {
            GENERATED_PROFILE_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
        })
        .filter(|name| !preserve.contains(*name))
        .cloned()
        .collect::<Vec<_>>();
    removed_profiles.sort();

    if removed_profiles.is_empty() {
        return Ok(removed_profiles);
    }

    let mut edits = removed_profiles
        .iter()
        .map(|profile| ConfigEdit::ClearPath {
            segments: vec!["profiles".to_string(), profile.clone()],
        })
        .collect::<Vec<_>>();
    if config.profiles.len() == removed_profiles.len() {
        edits.push(ConfigEdit::ClearPath {
            segments: vec!["profiles".to_string()],
        });
    }

    codex_core::config::edit::apply(codex_home, None, edits).await?;

    Ok(removed_profiles)
}

#[cfg(test)]
mod tests {
    use super::cleanup_generated_profiles;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    #[tokio::test]
    async fn cleanup_generated_profiles_removes_smart_prefix_entries() {
        let temp = tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[profiles.smart-old]\nmodel = \"gpt-5\"\n[profiles.keep]\nmodel = \"gpt-5.1\"\n",
        )
        .expect("write config");

        let result = cleanup_generated_profiles(temp.path(), &[Some("keep")])
            .await
            .expect("cleanup");

        assert_eq!(result, vec!["smart-old".to_string()]);
        let updated = std::fs::read_to_string(config_path).expect("read updated config");
        assert!(updated.contains("[profiles.keep]"));
        assert!(!updated.contains("[profiles.smart-old]"));
    }
}
