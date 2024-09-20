use anyhow::{anyhow, Context, Result};
use faer::Mat;
use faer_ext::polars::polars_to_faer_f32;
use log::info;
use polars::prelude::*;
use serde::{Deserialize, Serialize};
use sqlx::prelude::FromRow;
use std::fs::File;
use std::path::PathBuf;
use std::str::FromStr;
use std::{fmt::Display, path::Path};
use tokio::time::Instant;
use uuid::Uuid;

#[derive(Serialize, FromRow, Debug)]
pub struct CohortResponse {
    pub id: i32,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, FromRow, Serialize)]
pub struct Cohort {
    pub id: Option<i32>,
    pub name: String,
    pub normalized_name: String,
    pub num_covar: Option<i32>,
}

#[derive(Serialize, PartialEq, Clone, Copy, Debug, Deserialize, sqlx::Type)]
#[sqlx(rename_all = "UPPERCASE")]
pub enum NodeType {
    #[serde(rename = "BOOL")]
    Bool,
    #[serde(rename = "REAL")]
    Real,
    #[serde(rename = "ANY")]
    Any,
}

impl Display for NodeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let string = match self {
            NodeType::Bool => "BOOL",
            NodeType::Real => "REAL",
            NodeType::Any => "ANY",
        };
        write!(f, "{}", string)
    }
}

impl FromStr for NodeType {
    type Err = anyhow::Error;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        match input {
            "BOOL" => Ok(NodeType::Bool),
            "REAL" => Ok(NodeType::Real),
            "ANY" => Ok(NodeType::Any),
            _ => Err(anyhow!("Invalid node type: {}", input)),
        }
    }
}

#[derive(Serialize, FromRow, Debug)]
pub struct FeatureResponse {
    pub code: String,
    pub name: String,
    #[serde(rename = "type")]
    pub node_type: NodeType,
    pub sample_size: i32,
}

#[derive(Clone, Debug, PartialEq, FromRow, Serialize)]
pub struct Feature {
    pub id: i32,
    pub code: String,
    pub name: String,
    pub node_type: NodeType,
    pub sample_size: i32,
    pub cohort_id: i32,
}

#[derive(Serialize)]
pub struct ValidPhenotypeResponse {
    pub is_valid: bool,
    pub message: String,
    pub phenotype_definition: String,
}

pub struct ValidPhenotype {
    pub is_valid: bool,
    pub message: String,
    pub phenotype_definition: String,
    pub valid_nodes: Vec<Node>,
}

#[derive(Deserialize, sqlx::Type)]
pub struct PhenotypeSummaryRequest {
    pub phenotype_definition: String,
    pub cohort_id: i32,
    pub n_samples: Option<usize>,
}

#[derive(Deserialize, sqlx::Type)]
pub struct WebGWASRequest {
    pub phenotype_definition: String,
    pub cohort_id: i32,
}

pub struct WebGWASRequestId {
    pub id: Uuid,
    pub request_time: Instant,
    pub phenotype_definition: Vec<Node>,
    pub cohort_id: i32,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WebGWASResultStatus {
    Queued,
    Uploading,
    Done,
    Error,
}

#[derive(Serialize)]
pub struct WebGWASResponse {
    pub request_id: Uuid,
    pub status: WebGWASResultStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct WebGWASResult {
    pub request_id: Uuid,
    pub status: WebGWASResultStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_msg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing)]
    pub local_result_file: Option<PathBuf>,
}

#[derive(Serialize)]
pub struct PvaluesResponse {
    pub request_id: Uuid,
    pub status: WebGWASResultStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_msg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pvalues: Option<Vec<f32>>,
}

pub fn round_to_decimals<S>(value: &f32, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    serializer.serialize_f64((*value as f64 * 10000.0).trunc() / 10000.0)
}

#[derive(Serialize)]
pub struct ApproximatePhenotypeValues {
    #[serde(rename = "t", serialize_with = "round_to_decimals")]
    pub true_value: f32,
    #[serde(rename = "a", serialize_with = "round_to_decimals")]
    pub approx_value: f32,
    pub n: i32,
}

#[derive(Clone, Serialize)]
pub struct PhenotypeFitQuality {
    #[serde(rename = "p", serialize_with = "round_to_decimals")]
    pub phenotype_fit_quality: f32,
    #[serde(rename = "g", serialize_with = "round_to_decimals")]
    pub gwas_fit_quality: f32,
}

#[derive(Serialize)]
pub struct PhenotypeSummary {
    pub phenotype_definition: String,
    pub cohort_id: i32,
    pub phenotype_values: Vec<ApproximatePhenotypeValues>,
    pub fit_quality_reference: Vec<PhenotypeFitQuality>,
    #[serde(serialize_with = "round_to_decimals")]
    pub rsquared: f32,
}

#[derive(Clone, Debug)]
pub struct Operator {
    pub id: i32,
    pub name: String,
    pub arity: i32,
    pub input_type: NodeType,
    pub output_type: NodeType,
}

#[derive(Clone, Debug)]
pub struct Constant {
    pub value: f32,
    pub node_type: NodeType,
}

#[derive(Debug, Clone)]
pub enum Node {
    Feature(Feature),
    Operator(Operators),
    Constant(Constant),
}

#[derive(Deserialize)]
pub struct GetFeaturesRequest {
    pub cohort_id: i32,
}

pub struct CohortData {
    pub cohort: Cohort,
    pub features_df: DataFrame,
    pub left_inverse: Mat<f32>,
    pub gwas_df: DataFrame,
    pub covariance_matrix: Mat<f32>,
}

impl CohortData {
    pub fn load(cohort: Cohort, root_directory: &Path) -> Result<CohortData> {
        let cohort_root = root_directory.join("cohorts").join(&cohort.normalized_name);
        info!("Loading features for {}", cohort_root.display());
        let features_file_path = cohort_root.join("phenotypes.parquet");
        let features_file = File::open(features_file_path).context(anyhow!(
            "Failed to open phenotype data file for {}",
            cohort_root.display()
        ))?;
        let features_df = ParquetReader::new(features_file).finish()?;

        info!("Loading left inverse for {}", cohort_root.display());
        let left_inverse_file_path = cohort_root.join("phenotype_left_inverse.parquet");
        let left_inverse_file = File::open(left_inverse_file_path).context(anyhow!(
            "Failed to open left inverse file for {}",
            cohort_root.display()
        ))?;
        let left_inverse_df = ParquetReader::new(left_inverse_file).finish()?;
        let left_inverse = polars_to_faer_f32(left_inverse_df.lazy())?
            .transpose()
            .to_owned();

        info!("Loading GWAS for {}", cohort_root.display());
        let gwas_file_path = cohort_root.join("gwas.parquet");
        let gwas_file = File::open(gwas_file_path).context(anyhow!(
            "Failed to open GWAS file for {}",
            cohort_root.display()
        ))?;
        let gwas_df = ParquetReader::new(gwas_file).finish()?;

        info!("Loading covariance matrix for {}", cohort_root.display());
        let covariance_matrix_file_path = cohort_root.join("covariance.parquet");
        let covariance_matrix_file = File::open(covariance_matrix_file_path).context(anyhow!(
            "Failed to open covariance matrix file for {}",
            cohort_root.display()
        ))?;
        let covariance_matrix_df = ParquetReader::new(covariance_matrix_file).finish()?;
        let covariance_matrix = polars_to_faer_f32(covariance_matrix_df.lazy())?;
        info!("Finished loading cohort info for {}", cohort_root.display());

        Ok(CohortData {
            cohort,
            features_df,
            left_inverse,
            gwas_df,
            covariance_matrix,
        })
    }
}

#[derive(Copy, Clone, Debug)]
pub enum Operators {
    Root,
    Add,
    Sub,
    Mul,
    Div,
    And,
    Or,
    Not,
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
}

impl FromStr for Operators {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_uppercase().as_str() {
            "ROOT" => Ok(Operators::Root),
            "ADD" => Ok(Operators::Add),
            "SUB" => Ok(Operators::Sub),
            "MUL" => Ok(Operators::Mul),
            "DIV" => Ok(Operators::Div),
            "AND" => Ok(Operators::And),
            "OR" => Ok(Operators::Or),
            "NOT" => Ok(Operators::Not),
            "GT" => Ok(Operators::Gt),
            "GE" => Ok(Operators::Ge),
            "LT" => Ok(Operators::Lt),
            "LE" => Ok(Operators::Le),
            "EQ" => Ok(Operators::Eq),
            _ => Err(anyhow!("Invalid operator: {}", s)),
        }
    }
}

impl Operators {
    pub fn value(&self) -> Operator {
        match self {
            Operators::Root => Operator {
                id: 0,
                name: "root".to_string(),
                arity: 1,
                input_type: NodeType::Any,
                output_type: NodeType::Any,
            },
            Operators::Add => Operator {
                id: 1,
                name: "add".to_string(),
                arity: 2,
                input_type: NodeType::Any,
                output_type: NodeType::Real,
            },
            Operators::Sub => Operator {
                id: 2,
                name: "sub".to_string(),
                arity: 2,
                input_type: NodeType::Any,
                output_type: NodeType::Real,
            },
            Operators::Mul => Operator {
                id: 3,
                name: "mul".to_string(),
                arity: 2,
                input_type: NodeType::Any,
                output_type: NodeType::Real,
            },
            Operators::Div => Operator {
                id: 4,
                name: "div".to_string(),
                arity: 2,
                input_type: NodeType::Any,
                output_type: NodeType::Real,
            },
            Operators::And => Operator {
                id: 5,
                name: "and".to_string(),
                arity: 2,
                input_type: NodeType::Bool,
                output_type: NodeType::Bool,
            },
            Operators::Or => Operator {
                id: 6,
                name: "or".to_string(),
                arity: 2,
                input_type: NodeType::Bool,
                output_type: NodeType::Bool,
            },
            Operators::Not => Operator {
                id: 7,
                name: "not".to_string(),
                arity: 1,
                input_type: NodeType::Bool,
                output_type: NodeType::Bool,
            },
            Operators::Gt => Operator {
                id: 8,
                name: "gt".to_string(),
                arity: 2,
                input_type: NodeType::Any,
                output_type: NodeType::Bool,
            },
            Operators::Ge => Operator {
                id: 9,
                name: "ge".to_string(),
                arity: 2,
                input_type: NodeType::Any,
                output_type: NodeType::Bool,
            },
            Operators::Lt => Operator {
                id: 10,
                name: "lt".to_string(),
                arity: 2,
                input_type: NodeType::Any,
                output_type: NodeType::Bool,
            },
            Operators::Le => Operator {
                id: 11,
                name: "le".to_string(),
                arity: 2,
                input_type: NodeType::Any,
                output_type: NodeType::Bool,
            },
            Operators::Eq => Operator {
                id: 12,
                name: "eq".to_string(),
                arity: 2,
                input_type: NodeType::Any,
                output_type: NodeType::Bool,
            },
        }
    }
}

#[derive(Clone)]
pub enum ParsingNode {
    Feature(String),
    Operator(Operators),
    Constant(f32),
}

impl From<ParsingNode> for Node {
    fn from(parsing_node: ParsingNode) -> Self {
        match parsing_node {
            ParsingNode::Feature(field_code) => Node::Feature(Feature {
                id: 0,
                code: field_code,
                name: "".to_string(),
                node_type: NodeType::Any,
                sample_size: 0,
                cohort_id: 0,
            }),
            ParsingNode::Operator(operator) => Node::Operator(operator),
            ParsingNode::Constant(value) => Node::Constant(Constant {
                value,
                node_type: NodeType::Real,
            }),
        }
    }
}
