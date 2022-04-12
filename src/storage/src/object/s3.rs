// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::future::Future;

use aws_sdk_s3::{Client, Endpoint, Region};
use aws_smithy_http::body::SdkBody;
use futures::future::try_join_all;
use itertools::Itertools;
use risingwave_common::error::{BoxedError, ErrorCode, RwError};

use super::{BlockLocation, ObjectError, ObjectMetadata, ObjectResult};
use crate::object::{Bytes, ObjectStore};

/// Object store with S3 backend
pub struct S3ObjectStore {
    client: Client,
    bucket: String,
}

fn err(err: impl Into<BoxedError>) -> RwError {
    ErrorCode::StorageError(err.into()).into()
}

impl ObjectStore for S3ObjectStore {
    type EmptyFuture<'a> = impl Future<Output = ObjectResult<()>>;
    type BytesFuture<'a> = impl Future<Output = ObjectResult<Bytes>>;
    type BytesVecFuture<'a> = impl Future<Output = ObjectResult<Vec<Bytes>>>;
    type ObjectMetaFuture<'a> = impl Future<Output = ObjectResult<ObjectMetadata>>;
    type DeleteFuture<'a> = impl Future<Output = ObjectResult<()>>;

    fn upload<'a>(&'a self, path: &'a str, obj: Bytes) -> Self::EmptyFuture<'_> {
        async move {
            self.client
                .put_object()
                .bucket(&self.bucket)
                .body(SdkBody::from(obj).into())
                .key(path)
                .send()
                .await?;
            Ok(())
        }
    }

    /// Amazon S3 doesn't support retrieving multiple ranges of data per GET request.
    fn read<'a>(&'a self, path: &'a str, block_loc: Option<BlockLocation>) -> Self::BytesFuture<'a> {
        async move {
            let req = self.client.get_object().bucket(&self.bucket).key(path);

            let range = match block_loc.as_ref() {
                None => None,
                Some(block_location) => block_location.byte_range_specifier(),
            };

            let req = if let Some(range) = range {
                req.range(range)
            } else {
                req
            };

            let resp = req.send().await?;

            let val = resp.body.collect().await?.into_bytes();

            if block_loc.is_some() && block_loc.as_ref().unwrap().size != val.len() {
                return Err(ObjectError::internal(format!(
                    "mismatched size: expected {}, found {} when reading {} at {:?}",
                    block_loc.as_ref().unwrap().size,
                    val.len(),
                    path,
                    block_loc.as_ref().unwrap()
                )));
            }
            Ok(val)
        }
    }

    fn readv<'a>(&'a self, path: &'a str, block_locs: Vec<BlockLocation>) -> Self::BytesVecFuture<'a> {
        async move {
            let futures = block_locs
                .into_iter()
                .map(|block_loc| self.read(path, Some(block_loc)))
                .collect_vec();
            try_join_all(futures).await
        }
    }

    fn metadata<'a>(&'a self, path: &'a str) -> Self::ObjectMetaFuture<'_> {
        async move {
            let resp = self
                .client
                .head_object()
                .bucket(&self.bucket)
                .key(path)
                .send()
                .await?;
            Ok(ObjectMetadata {
                total_size: resp.content_length as usize,
            })
        }
    }

    /// Permanently deletes the whole object.
    /// According to Amazon S3, this will simply return Ok if the object does not exist.
    fn delete<'a>(&'a self, path: &'a str) -> Self::DeleteFuture<'_> {
        async move {
            self.client
                .delete_object()
                .bucket(&self.bucket)
                .key(path)
                .send()
                .await?;
            Ok(())
        }
    }
}

impl S3ObjectStore {
    /// Creates an S3 object store from environment variable.
    ///
    /// See [AWS Docs](https://docs.aws.amazon.com/sdk-for-rust/latest/dg/credentials.html) on how to provide credentials and region from env variable. If you are running compute-node on EC2, no configuration is required.
    pub async fn new(bucket: String) -> Self {
        let shared_config = aws_config::load_from_env().await;
        let client = Client::new(&shared_config);

        Self { client, bucket }
    }

    /// Creates a minio client. The server should be like `minio://key:secret@address:port/bucket`.
    pub async fn new_with_minio(server: &str) -> Self {
        let server = server.strip_prefix("minio://").unwrap();
        let (access_key_id, rest) = server.split_once(':').unwrap();
        let (secret_access_key, rest) = rest.split_once('@').unwrap();
        let (address, bucket) = rest.split_once('/').unwrap();

        let loader = aws_config::ConfigLoader::default();
        let builder = aws_sdk_s3::config::Builder::from(&loader.load().await);
        let builder = builder.region(Region::new("custom"));
        let builder = builder.endpoint_resolver(Endpoint::immutable(
            format!("http://{}", address).try_into().unwrap(),
        ));
        let builder = builder.credentials_provider(aws_sdk_s3::Credentials::from_keys(
            access_key_id,
            secret_access_key,
            None,
        ));
        let config = builder.build();
        let client = Client::from_conf(config);
        Self {
            client,
            bucket: bucket.to_string(),
        }
    }
}