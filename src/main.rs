mod chunk_db;
mod meta_db;
mod tls;

use anyhow::anyhow;
use bfsp::list_chunk_metadata_resp::ChunkMetadatas;
use bfsp::list_file_metadata_resp::FileMetadatas;
use biscuit_auth::datalog::RunLimits;
use biscuit_auth::PublicKey;
use biscuit_auth::{macros::authorizer, Authorizer, Biscuit};
use chunk_db::ChunkDB;
use std::env;
use std::fmt::Display;
use std::net::ToSocketAddrs;
use std::time::Duration;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};
use tokio::{fs, io};
use wtransport::endpoint::IncomingSession;
use wtransport::Endpoint;
use wtransport::Identity;
use wtransport::ServerConfig;

use crate::chunk_db::file::FSChunkDB;
use crate::chunk_db::s3::S3ChunkDB;
use crate::meta_db::{InsertChunkError, MetaDB, PostgresMetaDB};
use anyhow::Result;
use bfsp::{
    chunks_uploaded_query_resp::{ChunkUploaded, ChunksUploaded},
    download_chunk_resp::ChunkData,
    file_server_message::Message::{
        ChunksUploadedQuery, DeleteChunksQuery, DownloadChunkQuery, DownloadFileMetadataQuery,
        ListChunkMetadataQuery, ListFileMetadataQuery, UploadChunk, UploadFileMetadata,
    },
    ChunkID, ChunkMetadata, ChunksUploadedQueryResp, DownloadChunkResp, FileServerMessage, Message,
};
use bfsp::{EncryptedFileMetadata, EncryptionNonce, PrependLen};
use log::{debug, info, trace};
use tls::get_tls_cert;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() -> Result<()> {
    fern::Dispatch::new()
        .format(|out, msg, record| {
            out.finish(format_args!(
                "[{} {} {}] {}",
                humantime::format_rfc3339(std::time::SystemTime::now()),
                record.level(),
                record.target(),
                msg
            ))
        }) // Add blanket level filter -
        .level(
            cfg!(debug_assertions)
                .then(|| log::LevelFilter::Debug)
                .unwrap_or(log::LevelFilter::Info),
        )
        .level_for("sqlx", log::LevelFilter::Warn)
        .level_for("quinn_proto", log::LevelFilter::Warn)
        .level_for("quinn", log::LevelFilter::Warn)
        .level_for("rustls", log::LevelFilter::Warn)
        .level_for("tracing", log::LevelFilter::Warn)
        .level_for("wtransport", log::LevelFilter::Trace)
        // - and per-module overrides
        // Output to stdout, files, and other Dispatch configurations
        .chain(std::io::stdout())
        .chain(fern::log_file("output.log")?)
        // Apply globally
        .apply()?;

    let public_key = env::var("TOKEN_PUBLIC_KEY").unwrap();
    let public_key = PublicKey::from_bytes_hex(&public_key)?;

    info!("Initializing database");
    let meta_db = Arc::new(
        PostgresMetaDB::new()
            .await
            .map_err(|err| anyhow!("Error initializing database: {err:?}"))
            .unwrap(),
    );
    #[cfg(debug_assertions)]
    let chunk_db = Arc::new(FSChunkDB::new().unwrap());
    #[cfg(not(debug_assertions))]
    let chunk_db = Arc::new(S3ChunkDB::new().unwrap());

    chunk_db.garbage_collect(meta_db.clone()).await?;

    info!("Starting server!");

    let addr = match env::var("FLY_APP_NAME").is_ok() {
        // in order to serve Webtransport (UDP) on Fly, we have to use fly-global-services, which keep in mind is IPV4 ONLY AS OF WRITING
        true => "fly-global-services:9999"
            .to_socket_addrs()
            .unwrap()
            .next()
            .unwrap(),
        // I <3 ipv6
        false => "[::]:9999".to_socket_addrs().unwrap().next().unwrap(),
    };

    if !cfg!(debug_assertions) && env::var("FLY_APP_NAME").is_ok() {
        let cert_info = get_tls_cert().await?;

        fs::create_dir_all("/etc/letsencrypt/live/big-file-server.fly.dev/").await?;
        fs::write(
            "/etc/letsencrypt/live/big-file-server.fly.dev/chain.pem",
            cert_info.cert_chain_pem,
        )
        .await?;
        fs::write(
            "/etc/letsencrypt/live/big-file-server.fly.dev/privkey.pem",
            cert_info.private_key_pem,
        )
        .await?;
    }

    let chain_file = match cfg!(debug_assertions) {
        true => "certs/localhost.pem",
        false => "/etc/letsencrypt/live/big-file-server.fly.dev/chain.pem",
    };
    let key_file = match cfg!(debug_assertions) {
        true => "certs/localhost-key.pem",
        false => "/etc/letsencrypt/live/big-file-server.fly.dev/privkey.pem",
    };

    let config = ServerConfig::builder()
        .with_bind_address(addr)
        .with_identity(&Identity::load_pemfiles(chain_file, key_file).await.unwrap())
        .keep_alive_interval(Some(Duration::from_secs(3)))
        .allow_migration(true)
        .max_idle_timeout(Some(Duration::from_secs(10)))
        .unwrap()
        .build();

    let server = Endpoint::server(config).unwrap();

    info!("Listening on {addr}");

    loop {
        let incoming_session = server.accept().await;
        let meta_db = Arc::clone(&meta_db);
        let chunk_db = Arc::clone(&chunk_db);

        tokio::task::spawn(handle_connection(
            incoming_session,
            public_key,
            meta_db,
            chunk_db,
        ));
        debug!("Spawned connection task")
    }
}

async fn handle_connection<M: MetaDB + 'static, C: ChunkDB + 'static>(
    incoming_session: IncomingSession,
    public_key: PublicKey,
    meta_db: Arc<M>,
    chunk_db: Arc<C>,
) {
    info!("Transport connected");

    let session_request = incoming_session.await.unwrap();
    let conn = session_request.accept().await.unwrap();

    loop {
        let bi = conn.accept_bi().await;
        if let Err(err) = bi {
            debug!("Error accepting connection: {err}");
            return;
        }

        debug!("Accepted connection");

        // A single socket can have multiple connections. Multiplexing!
        let (mut write_sock, mut read_sock) = bi.unwrap();
        let meta_db = Arc::clone(&meta_db);
        let chunk_db = Arc::clone(&chunk_db);

        debug!("Bidirectionl connection established!");

        tokio::task::spawn(async move {
            loop {
                debug!("Waiting for message");
                let action_len = match read_sock.read_u32_le().await.map_err(|e| e.kind()) {
                    Ok(len) => len,
                    Err(io::ErrorKind::UnexpectedEof) => {
                        debug!("Client disconnected");
                        // This is fine, the client disconnected
                        return;
                    }
                    Err(err) => {
                        info!("Disconnecting from sock: {err}");
                        return;
                    }
                };

                debug!("Action is {action_len} bytes");

                // 9 MiB, super arbitrary
                if action_len > 9_437_184 {
                    todo!("Action {action_len} too big :(");
                }

                let command = {
                    let mut action_buf = vec![0; action_len as usize];
                    read_sock.read_exact(&mut action_buf).await.unwrap();
                    FileServerMessage::from_bytes(&action_buf).unwrap()
                };
                let authentication = command.auth.unwrap();
                debug!("Token: {}", authentication.token);

                let token = Biscuit::from_base64(&authentication.token, public_key).unwrap();

                let resp: Vec<u8> = match command.message.unwrap() {
                    DownloadChunkQuery(query) => {
                        match handle_download_chunk(
                            meta_db.as_ref(),
                            chunk_db.as_ref(),
                            &token,
                            ChunkID::try_from(query.chunk_id.as_str()).unwrap(),
                        )
                        .await
                        {
                            Ok(Some((meta, data))) => DownloadChunkResp {
                                response: Some(bfsp::download_chunk_resp::Response::ChunkData(
                                    ChunkData {
                                        chunk_metadata: Some(meta),
                                        chunk: data,
                                    },
                                )),
                            }
                            .encode_to_vec(),
                            Ok(None) => DownloadChunkResp {
                                response: Some(bfsp::download_chunk_resp::Response::Err(
                                    "ChunkNotFound".to_string(),
                                )),
                            }
                            .encode_to_vec(),
                            Err(_) => todo!(),
                        }
                    }
                    ChunksUploadedQuery(query) => {
                        let chunk_ids = query
                            .chunk_ids
                            .into_iter()
                            .map(|chunk_id| ChunkID::try_from(chunk_id.as_str()).unwrap())
                            .collect();
                        match query_chunks_uploaded(meta_db.as_ref(), &token, chunk_ids).await {
                            Ok(chunks_uploaded) => ChunksUploadedQueryResp {
                                response: Some(bfsp::chunks_uploaded_query_resp::Response::Chunks(
                                    ChunksUploaded {
                                        chunks: chunks_uploaded
                                            .into_iter()
                                            .map(|(chunk_id, uploaded)| ChunkUploaded {
                                                chunk_id: chunk_id.to_bytes().to_vec(),
                                                uploaded,
                                            })
                                            .collect(),
                                    },
                                )),
                            }
                            .encode_to_vec(),
                            Err(err) => todo!("Handle error: {err:?}"),
                        }
                    }
                    UploadChunk(msg) => {
                        let chunk_metadata = msg.chunk_metadata.unwrap();
                        let chunk = msg.chunk;

                        match handle_upload_chunk(meta_db.as_ref(), chunk_db.as_ref(), &token, chunk_metadata, &chunk).await
                        {
                            Ok(_) => bfsp::UploadChunkResp { err: None }.encode_to_vec(),
                            Err(err) => todo!("{err}"),
                        }
                    }
                    DeleteChunksQuery(query) => {
                        let chunk_ids: HashSet<ChunkID> = query
                            .chunk_ids
                            .into_iter()
                            .map(|chunk_id| ChunkID::try_from(chunk_id.as_str()).unwrap())
                            .collect();

                        match handle_delete_chunks(meta_db.as_ref(), chunk_db.as_ref(), &token, chunk_ids).await {
                            Ok(_) => bfsp::DeleteChunksResp { err: None }.encode_to_vec(),
                            Err(err) => todo!("{err}"),
                        }
                    }
                    UploadFileMetadata(meta) => {
                        let encrypted_file_meta = meta.encrypted_file_metadata.unwrap();
                        match handle_upload_file_metadata(meta_db.as_ref(), &token, encrypted_file_meta)
                            .await
                        {
                            Ok(_) => bfsp::UploadFileMetadataResp { err: None }.encode_to_vec(),
                            Err(err) => todo!("{err:?}"),
                        }
                    }
                    DownloadFileMetadataQuery(query) => {
                        let meta_id = query.id;
                        match handle_download_file_metadata(meta_db.as_ref(), &token, meta_id).await {
                        Ok(meta) => bfsp::DownloadFileMetadataResp {
                            response: Some(
                                bfsp::download_file_metadata_resp::Response::EncryptedFileMetadata(
                                    meta,
                                ),
                            ),
                        }
                        .encode_to_vec(),
                        Err(_) => todo!(),
                    }
                    }
                    ListFileMetadataQuery(query) => {
                        let meta_ids = query.ids;
                        match handle_list_file_metadata(meta_db.as_ref(), &token, meta_ids).await {
                            Ok(metas) => bfsp::ListFileMetadataResp {
                                response: Some(bfsp::list_file_metadata_resp::Response::Metadatas(
                                    FileMetadatas { metadatas: metas },
                                )),
                            }
                            .encode_to_vec(),
                            Err(_) => todo!(),
                        }
                    }
                    ListChunkMetadataQuery(query) => {
                        let meta_ids = query.ids;
                        match handle_list_chunk_metadata(meta_db.as_ref(), &token, meta_ids).await {
                            Ok(metas) => bfsp::ListChunkMetadataResp {
                                response: Some(bfsp::list_chunk_metadata_resp::Response::Metadatas(
                                    ChunkMetadatas { metadatas: metas.into_iter().map(|(chunk_id, chunk_meta)| (chunk_id.to_string(), chunk_meta)).collect()},
                                )),
                            }
                            .encode_to_vec(),
                            Err(_) => todo!(),
                        }

                    },
                }
                .prepend_len();

                debug!("Sending response of {} bytes", resp.len());
                write_sock.write_all(&resp).await.unwrap();
                write_sock.flush().await.unwrap();

                debug!("Sent response");
            }
        });
    }
}

pub async fn handle_download_chunk<M: MetaDB, C: ChunkDB>(
    meta_db: &M,
    chunk_db: &C,
    token: &Biscuit,
    chunk_id: ChunkID,
) -> Result<Option<(ChunkMetadata, Vec<u8>)>> {
    let mut authorizer = authorizer!(
        r#"
            check if user($user);
            check if rights($rights), $rights.contains("read");
            allow if true;
            deny if false;
        "#
    );
    authorizer.add_token(token).unwrap();

    let mut authorizer_clone = authorizer.clone();
    let authorize: anyhow::Result<()> = tokio::task::spawn_blocking(move || {
        authorizer_clone.authorize().unwrap();

        Ok(())
    })
    .await
    .unwrap();

    authorize.unwrap();

    info!("Downloading chunk {}", chunk_id);

    let user_id = get_user_id(&mut authorizer).unwrap();

    let chunk_meta = match meta_db.get_chunk_meta(chunk_id, user_id).await? {
        Some(chunk_meta) => chunk_meta,
        None => return Ok(None),
    };

    let chunk = chunk_db.get_chunk(&chunk_id, user_id).await?;
    debug!("Sending chunk {chunk_id}");
    match chunk {
        Some(chunk) => Ok(Some((chunk_meta, chunk))),
        None => return Ok(None),
    }
}

// FIXME: very ddosable by querying many chunks at once
async fn query_chunks_uploaded<M: MetaDB>(
    meta_db: &M,
    token: &Biscuit,
    chunks: HashSet<ChunkID>,
) -> Result<HashMap<ChunkID, bool>> {
    let mut authorizer = authorizer!(
        r#"
            check if user($user);
            check if rights($rights), $rights.contains("query");
            allow if true;
            deny if false;
        "#
    );

    authorizer.add_token(token).unwrap();
    authorizer.authorize().unwrap();

    let user_id = get_user_id(&mut authorizer).unwrap();

    let chunks_uploaded: HashMap<ChunkID, bool> =
        futures::future::join_all(chunks.into_iter().map(|chunk_id| async move {
            let contains_chunk: bool = meta_db
                .contains_chunk_meta(chunk_id, user_id)
                .await
                .unwrap();
            (chunk_id, contains_chunk)
        }))
        .await
        .into_iter()
        .collect();

    Ok(chunks_uploaded)
}

// TODO: Maybe store upload_chunk messages in files and mmap them?
async fn handle_upload_chunk<M: MetaDB, C: ChunkDB>(
    meta_db: &M,
    chunk_db: &C,
    token: &Biscuit,
    chunk_metadata: ChunkMetadata,
    chunk: &[u8],
) -> Result<()> {
    trace!("Handling chunk upload");

    let mut authorizer = authorizer!(
        r#"
            check if user($user);
            check if rights($rights), $rights.contains("write");
            allow if true;
            deny if false;
        "#
    );

    let mut added_token = authorizer.add_token(token).is_ok();
    while !added_token {
        added_token = authorizer.add_token(token).is_ok();
    }

    authorizer
        .authorize_with_limits(RunLimits {
            max_time: Duration::from_secs(60),
            ..Default::default()
        })
        .unwrap();

    let user_id = get_user_id(&mut authorizer).unwrap();

    // 8MiB(?)
    if chunk_metadata.size > 1024 * 1024 * 8 {
        todo!("Deny uploads larger than our max chunk size");
    }

    if chunk_metadata.nonce.len() != EncryptionNonce::len() {
        todo!("Deny uploads with nonced_key != 32 bytes");
    }

    let chunk_id = ChunkID::try_from(chunk_metadata.id.as_str()).unwrap();
    trace!("Got chunk id");

    if let Err(err) = meta_db.insert_chunk_meta(chunk_metadata, user_id).await {
        if let InsertChunkError::AlreadyExists = err {
            // If the chunk already exists, no point in re-uploading it. Just tell the user we processed it :)
            return Ok(());
        } else {
            return Err(err.into());
        }
    };

    chunk_db.put_chunk(&chunk_id, user_id, chunk).await.unwrap();

    Ok(())
}

pub async fn handle_delete_chunks<D: MetaDB, C: ChunkDB>(
    meta_db: &D,
    chunk_db: &C,
    token: &Biscuit,
    chunk_ids: HashSet<ChunkID>,
) -> Result<()> {
    trace!("Handling delete chunk");

    let mut authorizer = authorizer!(
        r#"
            check if user($user);
            check if rights($rights), $rights.contains("delete");
            allow if true;
            deny if false;
        "#
    );

    authorizer.add_token(token).unwrap();
    authorizer.authorize().unwrap();

    let user_id = get_user_id(&mut authorizer).unwrap();

    let remove_chunk_files = chunk_ids.clone().into_iter().map(|chunk_id| async move {
        let chunk_id = chunk_id.clone();
        // TODO: delete multiple chunks at once
        chunk_db.delete_chunk(&chunk_id, user_id).await
    });

    tokio::join!(
        async move {
            futures::future::join_all(remove_chunk_files).await;
        },
        async move {
            meta_db.delete_chunk_metas(&chunk_ids).await.unwrap();
        },
    );

    Ok(())
}

#[derive(Debug)]
pub enum UploadMetadataError {
    MultipleUserIDs,
}

pub async fn handle_upload_file_metadata<D: MetaDB>(
    meta_db: &D,
    token: &Biscuit,
    enc_file_meta: EncryptedFileMetadata,
) -> Result<(), UploadMetadataError> {
    debug!("Handling file metadata upload");

    let mut authorizer = authorizer!(
        r#"
            check if user($user);
            check if rights($rights), $rights.contains("write");
            allow if true;
            deny if false;
        "#
    );

    authorizer.add_token(token).unwrap();
    authorizer.authorize().unwrap();

    let user_id = get_user_id(&mut authorizer).unwrap();
    debug!("Uploading metadata for user {}", user_id);

    meta_db
        .insert_file_meta(enc_file_meta, user_id)
        .await
        .unwrap();

    Ok(())
}

pub async fn handle_download_file_metadata<D: MetaDB>(
    meta_db: &D,
    token: &Biscuit,
    meta_id: String,
) -> Result<EncryptedFileMetadata, UploadMetadataError> {
    let mut authorizer = authorizer!(
        r#"
            check if user($user);
            check if rights($rights), $rights.contains("read");
            allow if true;
            deny if false;
        "#
    );

    authorizer.add_token(token).unwrap();
    authorizer.authorize().unwrap();

    let user_id = get_user_id(&mut authorizer).unwrap();
    match meta_db.get_file_meta(meta_id, user_id).await.unwrap() {
        Some(meta) => Ok(meta),
        None => Err(todo!()),
    }
}

pub async fn handle_list_file_metadata<D: MetaDB>(
    meta_db: &D,
    token: &Biscuit,
    meta_ids: Vec<String>,
) -> Result<HashMap<String, EncryptedFileMetadata>, UploadMetadataError> {
    info!("Listing metadata");
    let mut authorizer = authorizer!(
        r#"
            check if user($user);
            check if rights($rights), $rights.contains("query");
            allow if true;
            deny if false;
        "#
    );

    authorizer.add_token(token).unwrap();
    authorizer.authorize().unwrap();

    let meta_ids: HashSet<String> = HashSet::from_iter(meta_ids.into_iter());

    let user_id = get_user_id(&mut authorizer).unwrap();
    info!("Listing metadata for user {}", user_id);
    let meta = meta_db.list_file_meta(meta_ids, user_id).await.unwrap();
    Ok(meta)
}

pub async fn handle_list_chunk_metadata<D: MetaDB>(
    meta_db: &D,
    token: &Biscuit,
    meta_ids: Vec<String>,
) -> Result<HashMap<ChunkID, ChunkMetadata>, UploadMetadataError> {
    info!("Listing metadata");
    let mut authorizer = authorizer!(
        r#"
            check if user($user);
            check if rights($rights), $rights.contains("query");
            allow if true;
            deny if false;
        "#
    );

    authorizer.add_token(token).unwrap();
    authorizer.authorize().unwrap();

    let chunk_ids: HashSet<ChunkID> = meta_ids
        .into_iter()
        .map(|chunk_id| ChunkID::try_from(chunk_id.as_str()).unwrap())
        .collect();

    let user_id = get_user_id(&mut authorizer).unwrap();
    info!("Listing metadata for user {}", user_id);
    let meta = meta_db.list_chunk_meta(chunk_ids, user_id).await.unwrap();
    Ok(meta)
}

#[derive(thiserror::Error, Debug)]
pub enum GetUserIDError {
    MultipleUserIDs,
}

impl Display for GetUserIDError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("multipe user ids")
    }
}

pub fn get_user_id(authorizer: &mut Authorizer) -> Result<i64, GetUserIDError> {
    let user_info: Vec<(String,)> = authorizer
        .query_with_limits(
            "data($0) <- user($0)",
            RunLimits {
                max_time: Duration::from_secs(60),
                ..Default::default()
            },
        )
        .unwrap();

    debug!("{user_info:#?}");

    if user_info.len() != 1 {
        return Err(GetUserIDError::MultipleUserIDs);
    }

    let user_id: i64 = user_info.first().unwrap().0.parse().unwrap();
    Ok(user_id)
}
