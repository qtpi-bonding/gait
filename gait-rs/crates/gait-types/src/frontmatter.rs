use schemars::JsonSchema;
use serde::{Deserialize, Deserializer};

/// Frontmatter of `agents/<name>/AGENT.md`; the markdown body is the agent's role prompt.
#[derive(Debug, Default, Clone, Deserialize, JsonSchema)]
pub struct AgentFrontmatter {
    /// Whitelist of registered skills the agent may see (default-deny: absent means none).
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub skills: Vec<String>,
    /// Whitelist of registered MCP servers the agent may see (default-deny: absent means none).
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub mcp: Vec<String>,
    /// Whitelist of registered plugins the agent may see (default-deny: absent means none).
    #[serde(default, deserialize_with = "deserialize_string_list")]
    pub plugins: Vec<String>,
    /// Name of an inference profile the agent runs with.
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub inference: Option<String>,
}

impl AgentFrontmatter {
    /// Parse leniently, returning default frontmatter for malformed or non-mapping YAML.
    pub fn parse(fm: &str) -> Self {
        match serde_yaml::from_str(fm) {
            Ok(value) => value,
            Err(_) => Self::default(),
        }
    }
}

/// Frontmatter describing a loadable skill.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SkillFrontmatter {
    /// Required non-empty skill name.
    #[serde(default, deserialize_with = "deserialize_string")]
    pub name: String,
    /// Description shown to the model.
    #[serde(default, deserialize_with = "deserialize_string")]
    pub description: String,
    /// Whether the model may invoke this skill; omitted values default to true.
    #[serde(default = "default_model_invocable")]
    #[schemars(with = "Option<bool>")]
    pub model_invocable: Option<serde_yaml::Value>,
}

fn default_model_invocable() -> Option<serde_yaml::Value> {
    None
}

impl SkillFrontmatter {
    /// Parse skill frontmatter, returning none for malformed YAML.
    pub fn parse(fm: &str) -> Option<Self> {
        serde_yaml::from_str(fm).ok()
    }
}

fn deserialize_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_yaml::Value::deserialize(deserializer)?;
    Ok(value.as_str().unwrap_or("").to_string())
}

fn deserialize_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_yaml::Value::deserialize(deserializer)?;
    Ok(value.as_str().map(ToOwned::to_owned))
}

fn deserialize_string_list<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_yaml::Value::deserialize(deserializer)?;
    Ok(value
        .as_sequence()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::schema_for;

    #[test]
    fn lenient_agent_values_are_filtered() {
        let agent = AgentFrontmatter::parse("skills: [one, 2, three]\nmcp: nope\ninference: 3");
        assert_eq!(agent.skills, ["one", "three"]);
        assert!(agent.mcp.is_empty());
        assert_eq!(agent.inference, None);
    }

    #[test]
    fn malformed_agent_yaml_defaults() {
        let parsed = AgentFrontmatter::parse("skills: [");
        assert!(parsed.skills.is_empty());
        assert!(parsed.mcp.is_empty());
        assert!(parsed.plugins.is_empty());
        assert!(parsed.inference.is_none());
    }

    #[test]
    fn skill_preserves_non_boolean_model_invocable() {
        let skill = SkillFrontmatter::parse("name: x\nmodel_invocable: yes\n").unwrap();
        assert_eq!(skill.model_invocable.unwrap().as_str(), Some("yes"));
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let skill = SkillFrontmatter::parse("name: x\nextra: value\n").unwrap();
        assert_eq!(skill.name, "x");
    }

    #[test]
    fn every_schema_property_has_a_description() {
        let agent = schema_for!(AgentFrontmatter);
        let skill = schema_for!(SkillFrontmatter);
        for schema in [agent, skill] {
            let object = schema.schema.object.unwrap();
            assert!(object.properties.values().all(|property| match property {
                schemars::schema::Schema::Object(property) => property
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.description.as_ref())
                    .is_some(),
                _ => false,
            }));
        }
    }
}
