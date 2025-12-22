use serde::Serialize;

#[derive(Serialize)]
pub struct Manifest {
    pub id: String,
    pub version: String,
    pub name: String,
    pub description: String,
    pub resources: Vec<Resource>,
    pub types: Vec<String>,
    pub catalogs: Vec<Catalog>,
    #[serde(rename = "idPrefixes")]
    pub id_prefixes: Option<Vec<String>>,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum Resource {
    Short(String),
    Full(ResourceObject),
}

#[derive(Serialize)]
pub struct ResourceObject {
    pub name: String,
    pub types: Vec<String>,
    #[serde(rename = "idPrefixes")]
    pub id_prefixes: Option<Vec<String>>,
}

#[derive(Serialize)]
pub struct Catalog {
    pub r#type: String,
    pub id: String,
    pub name: String,
}

pub fn get_manifest() -> Manifest {
    Manifest {
        id: "org.stremio.local.rust".to_string(),
        version: "1.0.0".to_string(),
        name: "Local Files (Rust)".to_string(),
        description: "Local files served by Rust Server".to_string(),
        resources: vec![
            Resource::Short("catalog".to_string()),
            Resource::Full(ResourceObject {
                name: "meta".to_string(),
                types: vec!["movie".to_string(), "series".to_string(), "other".to_string()],
                id_prefixes: Some(vec!["local:".to_string(), "bt:".to_string()]),
            }),
            Resource::Full(ResourceObject {
                name: "stream".to_string(),
                types: vec!["movie".to_string(), "series".to_string(), "other".to_string()],
                id_prefixes: Some(vec!["local:".to_string(), "bt:".to_string()]),
            }),
        ],
        types: vec!["movie".to_string(), "series".to_string(), "other".to_string()],
        catalogs: vec![
            Catalog {
                r#type: "movie".to_string(),
                id: "local_movies".to_string(),
                name: "Local Movies".to_string(),
            },
            Catalog {
                r#type: "series".to_string(),
                id: "local_series".to_string(),
                name: "Local Series".to_string(),
            },
        ],
        id_prefixes: Some(vec!["local:".to_string(), "bt:".to_string()]),
    }
}
