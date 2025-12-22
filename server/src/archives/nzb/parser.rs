use serde::Deserialize;
use anyhow::Result;

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "lowercase")]
pub struct Nzb {
    #[serde(rename = "file", default)]
    pub files: Vec<NzbFile>,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct NzbFile {
    #[serde(rename = "@poster")]
    pub poster: Option<String>,
    #[serde(rename = "@date")]
    pub date: Option<u64>,
    #[serde(rename = "@subject")]
    pub subject: String,
    
    #[serde(rename = "groups")]
    pub groups: NzbGroups,
    
    #[serde(rename = "segments")]
    pub segments: NzbSegments,
}

#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct NzbGroups {
    #[serde(rename = "group", default)]
    pub groups: Vec<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct NzbSegments {
    #[serde(rename = "segment", default)]
    pub segments: Vec<NzbSegment>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct NzbSegment {
    #[serde(rename = "@bytes")]
    pub bytes: u64,
    #[serde(rename = "@number")]
    pub number: u32,
    #[serde(rename = "$value")]
    pub id: String,
}

pub fn parse_nzb_xml(xml: &str) -> Result<Nzb> {
    quick_xml::de::from_str(xml).map_err(|e| anyhow::anyhow!("Failed to parse NZB XML: {}", e))
}
