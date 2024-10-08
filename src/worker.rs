use anyhow::{bail, Context, Result};
use aws_sdk_s3::presigning::PresigningConfig;
use faer::Col;
use log::info;
use std::fs::File;
use std::io::{BufReader, Seek, Write};
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use tokio::time::Duration;
use tracing::info_span;
use zip::write::SimpleFileOptions;
use zip::CompressionMethod;

use crate::igwas::{run_igwas_df_impl, Projection};
use crate::models::{CohortData, Node, RequestMetadata};
use crate::phenotype_definitions::format_phenotype_definition;
use crate::regression::regress_left_inverse_vec;
use crate::utils::vec_to_col;
use crate::AppState;
use crate::{
    models::{WebGWASRequestId, WebGWASResultStatus},
    phenotype_definitions::apply_phenotype_definition,
};

pub fn worker_loop(state: Arc<AppState>) {
    loop {
        let task = {
            let mut queue = state.queue.lock().unwrap();
            queue.pop()
        };
        if let Some(request) = task {
            let _span = info_span!("main_worker_loop", request_id = %request.id,
            )
            .entered();
            let result = handle_webgwas_request(state.clone(), request);
            if let Err(err) = result {
                info!("Failed to handle request: {}", err);
            }
        } else {
            thread::sleep(Duration::from_millis(10));
        }
    }
}

pub fn handle_webgwas_request(state: Arc<AppState>, request: WebGWASRequestId) -> Result<()> {
    // 0. Load the cohort info (relevant data for this request)
    let cohort_info = {
        let binding = state.cohort_id_to_data.lock().unwrap();
        binding
            .get(&request.cohort_id)
            .context(format!(
                "Failed to get cohort info for {}",
                request.cohort_id
            ))?
            .clone()
    };

    // 1. Apply the phenotype and compute the projection coefficents
    let projection_result = compute_projection(&request.phenotype_definition, &cohort_info);
    let mut projection = match projection_result {
        Ok(projection) => projection,
        Err(err) => {
            let mut results = state.results.lock().unwrap();
            let result = results
                .get_mut(&request.id)
                .context("Failed to get result")?;
            result.status = WebGWASResultStatus::Error;
            result.error_msg = Some(format!("Failed to compute projection: {}", err));
            return Err(err);
        }
    };

    // 2. Compute the projection variance
    let beta = &projection.feature_coefficient;
    let projection_variance = beta.transpose() * &cohort_info.covariance_matrix * beta;

    // 3. Compute GWAS
    let output_path = state
        .root_directory
        .join(format!("results/{}.tsv", request.id));
    {
        let _span = info_span!("run_igwas_df_impl").entered();
        run_igwas_df_impl(
            &cohort_info.gwas_df,
            &mut projection,
            projection_variance,
            cohort_info.cohort.num_covar.expect("Num_covar is missing") as usize,
            &output_path,
            16,
        )?;
    }
    {
        let mut results = state.results.lock().unwrap();
        let result = results
            .get_mut(&request.id)
            .context("Failed to get result")?;
        result.status = WebGWASResultStatus::Uploading;
        result.local_result_file = Some(output_path.clone());
    }

    let metadata_file = create_metadata_file(&state, &request)?;
    let output_zip_path = create_output_zip(&output_path, &metadata_file)?;
    std::fs::remove_file(metadata_file)?;

    let url = if state.settings.dry_run {
        info!("Dry run, skipping S3 upload");
        None
    } else {
        let _span = info_span!("upload_and_get_url").entered();
        let key = format!("{}/{}.zip", state.settings.s3_result_path, request.id);
        let url = upload_and_get_url(&state, &output_zip_path, &key)?;
        std::fs::remove_file(output_zip_path)?;
        Some(url)
    };
    {
        let mut results = state.results.lock().unwrap();
        let result = results.get_mut(&request.id).context("Result not found")?;
        result.status = WebGWASResultStatus::Done;
        result.url = url;
    }
    Ok(())
}

pub fn compute_projection(
    phenotype_definition: &[Node],
    cohort_info: &CohortData,
) -> Result<Projection> {
    if phenotype_definition.len() == 1 {
        match &phenotype_definition[0] {
            Node::Feature(feature) => {
                let mut beta = Col::zeros(1);
                beta[0] = 1.0;
                let phenotype_names = vec![feature.code.clone()];
                let mut projection = Projection::new(phenotype_names, beta)?;
                // Standardize to the full feature names
                projection.standardize(&cohort_info.feature_names);
                if !projection.feature_id.contains(&feature.code) {
                    bail!("Feature {} not found after standardization", feature.code);
                }
                Ok(projection)
            }
            Node::Operator(operator) => {
                bail!("Operator {} is not supported", operator.value().name);
            }
            Node::Constant(constant) => {
                bail!("Constant {} is not supported", constant.value);
            }
        }
    } else {
        let phenotype = apply_phenotype_definition(
            phenotype_definition,
            &cohort_info.feature_names,
            &cohort_info.features,
        )
        .context("Failed to apply phenotype definition")?;
        let phenotype_mat = vec_to_col(&phenotype);
        let beta = {
            let _span = info_span!("regress_left_inverse_vec").entered();
            let mut beta = regress_left_inverse_vec(&phenotype_mat, &cohort_info.left_inverse);
            beta.truncate(beta.nrows() - 1); // Drop the last element (the intercept)
            beta
        };
        let projection = Projection::new(cohort_info.feature_names.clone(), beta)?;
        Ok(projection)
    }
}

pub async fn upload_object(
    client: &aws_sdk_s3::Client,
    file_name: &Path,
    bucket_name: &str,
    key: &str,
) -> Result<aws_sdk_s3::operation::put_object::PutObjectOutput> {
    let body = aws_sdk_s3::primitives::ByteStream::from_path(file_name).await?;
    let result = client
        .put_object()
        .bucket(bucket_name)
        .key(key)
        .body(body)
        .send()
        .await?;
    Ok(result)
}

pub fn upload_and_get_url(state: &AppState, output_zip_path: &Path, key: &str) -> Result<String> {
    let rt = tokio::runtime::Runtime::new()?;
    let url = rt.block_on(async { upload_and_get_url_async(state, output_zip_path, key).await })?;
    Ok(url)
}

async fn upload_and_get_url_async(
    state: &AppState,
    output_zip_path: &Path,
    key: &str,
) -> Result<String> {
    upload_object(
        &state.s3_client,
        output_zip_path,
        &state.settings.s3_bucket,
        key,
    )
    .await
    .context("Failed to upload object")?;
    const URL_EXPIRES_IN: Duration = Duration::from_secs(3600);
    let url = state
        .s3_client
        .get_object()
        .bucket(&state.settings.s3_bucket)
        .key(key)
        .presigned(PresigningConfig::expires_in(URL_EXPIRES_IN)?)
        .await
        .context("Failed to get presigned URL")?
        .uri()
        .to_string();
    Ok(url)
}

pub fn create_metadata_file(state: &AppState, request: &WebGWASRequestId) -> Result<PathBuf> {
    let cohort_info = {
        let binding = state.cohort_id_to_data.lock().unwrap();
        binding
            .get(&request.cohort_id)
            .context(format!(
                "Failed to get cohort info for {}",
                request.cohort_id
            ))?
            .clone()
    };
    let metadata = RequestMetadata::new(
        request.id,
        format_phenotype_definition(&request.phenotype_definition),
        cohort_info.cohort.name.clone(),
        cohort_info.features.nrows(),
    );
    let output_metadata_path = state
        .root_directory
        .join(format!("results/{}.txt", request.id));
    let mut metadata_file = File::create(output_metadata_path.clone())?;
    write!(metadata_file, "{}", metadata)?;
    Ok(output_metadata_path)
}

pub fn add_file_to_zip<W>(
    zip_writer: &mut zip::ZipWriter<W>,
    file_path: &Path,
    name_in_zip: &str,
) -> zip::result::ZipResult<()>
where
    W: Write + Seek,
{
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(0o644);
    let file = File::open(file_path)?;
    let mut buffered_reader = BufReader::new(file);
    zip_writer.start_file(name_in_zip, options)?;
    std::io::copy(&mut buffered_reader, zip_writer)?;
    Ok(())
}

pub fn create_output_zip(output_path: &Path, metadata_path: &Path) -> Result<PathBuf> {
    let output_zip_path = output_path.with_extension("").with_extension("zip");
    let mut zip_writer = zip::ZipWriter::new(File::create(output_zip_path.clone())?);
    add_file_to_zip(&mut zip_writer, output_path, "results.tsv")?;
    add_file_to_zip(&mut zip_writer, metadata_path, "metadata.txt")?;
    zip_writer.finish()?;
    Ok(output_zip_path)
}
