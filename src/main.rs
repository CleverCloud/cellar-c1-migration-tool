mod migrate;
mod radosgw;
mod riakcs;

use bytesize::ByteSize;
use clap::{App, AppSettings, Arg, ArgMatches};
use migrate::BucketMigrationConfiguration;
use tracing::event;
use tracing::instrument;
use tracing::Level;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::EnvFilter;

use crate::migrate::{BucketMigrationError, BucketMigrationStats};
use crate::riakcs::dto::ObjectContents;
use crate::riakcs::RiakCS;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            std::env::var(EnvFilter::DEFAULT_ENV)
                .map(|_| EnvFilter::from_default_env())
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_span_events(FmtSpan::CLOSE | FmtSpan::NEW)
        .with_test_writer()
        .try_init();

    let num_cpus = num_cpus::get();
    let clap = clap::app_from_crate!()
        .setting(AppSettings::ArgRequiredElseHelp)
        .subcommand(
            App::new("migrate")
            .about("Migrate a cellar-c1 bucket to a cellar-c2 cluster. By default, it will dry run unless --execute is passed")
            .arg(Arg::new("source-bucket").long("source-bucket").help("Source bucket from which files will be copied. If omitted, all buckets of the add-on will be synchronized").takes_value(true))
            .arg(Arg::new("source-access-key").long("source-access-key").help("Source bucket Cellar access key").required(true).takes_value(true))
            .arg(Arg::new("source-secret-key").long("source-secret-key").help("Source bucket Cellar secret key").required(true).takes_value(true))
            .arg(Arg::new("destination-bucket").long("destination-bucket").help("Destination bucket to which the files will be copied. If omitted, the bucket will be created if it doesn't exist").takes_value(true))
            .arg(Arg::new("destination-bucket-prefix").long("destination-bucket-prefix").help("Prefix to apply to the destination bucket name").takes_value(true))
            .arg(Arg::new("destination-access-key").long("destination-access-key").help("Destination bucket Cellar access key").required(true).takes_value(true))
            .arg(Arg::new("destination-secret-key").long("destination-secret-key").help("Destination bucket Cellar secret key").required(true).takes_value(true))
            .arg(Arg::new("destination-endpoint").long("destination-endpoint").help("Destination endpoint of the Cellar cluster. Defaults to Paris Cellar cluster")
                .required(false).takes_value(true).default_value("cellar-c2.services.clever-cloud.com")
            )
            .arg(
                Arg::new("threads").long("threads").short('t').help("Number of threads used to synchronize this bucket")
                .required(false).takes_value(true).default_value(&num_cpus.to_string())
            )
            .arg(
                Arg::new("multipart-chunk-size-mb").long("multipart-chunk-size-mb")
                .help("Size of each chunk of multipart upload in Megabytes. Files bigger than this size are automatically uploaded using multipart upload")
                .required(false).takes_value(true).default_value("100")
            )
            .arg(
                Arg::new("execute").long("execute").short('e')
                .help("Execute the synchronization. THIS COMMAND WILL MAKE PRODUCTION CHANGES TO THE DESTINATION BUCKET.")
                .required(false).takes_value(false)
            )
            .arg(
                Arg::new("max-keys").long("max-keys").short('m')
                .help("Define the maximum number of object keys to list when listing the bucket. Lowering this might help listing huge buckets")
                .required(false).takes_value(true).default_value("1000")
            )
            .arg(
                Arg::new("delete").long("delete").short('d')
                .help("Delete extraneous files from destination bucket")
                .required(false).takes_value(false)
            )
        )
        .get_matches();

    match clap.subcommand() {
        Some(("migrate", migrate_matches)) => migrate_command(migrate_matches).await,
        e => unreachable!("Failed to parse subcommand: {:#?}", e),
    }
}

#[instrument(skip_all, level = "debug")]
async fn migrate_command(params: &ArgMatches) -> anyhow::Result<()> {
    let dry_run = params.occurrences_of("execute") == 0;

    if dry_run {
        event!(Level::WARN, "Running in dry run mode. No changes will be made. If you want to synchronize for real, use --execute");
    }

    let sync_threads = params
        .value_of_t("threads")
        .expect("Threads should be a usize");
    let multipart_upload_chunk_size: usize = params
        .value_of_t::<usize>("multipart-chunk-size-mb")
        .expect("Multipart chunk size should be a usize")
        * 1024
        * 1024;
    let max_keys = params
        .value_of_t::<usize>("max-keys")
        .expect("max-keys should be a usize");

    let delete_destination_files = params.occurrences_of("delete") > 0;

    let source_bucket = params.value_of("source-bucket").map(|b| b.to_string());
    let source_access_key = params.value_of("source-access-key").unwrap().to_string();
    let source_secret_key = params.value_of("source-secret-key").unwrap().to_string();
    let source_endpoint = "cellar.services.clever-cloud.com".to_string();

    let destination_bucket = params.value_of("destination-bucket").map(|b| b.to_string());
    let destination_bucket_prefix = params
        .value_of("destination-bucket-prefix")
        .map(|b| format!("{}-", b))
        .unwrap_or_default();
    let destination_access_key = params
        .value_of("destination-access-key")
        .unwrap()
        .to_string();
    let destination_secret_key = params
        .value_of("destination-secret-key")
        .unwrap()
        .to_string();
    let destination_endpoint = params.value_of("destination-endpoint").unwrap().to_string();

    if source_bucket.is_none() && destination_bucket.is_some() {
        event!(Level::ERROR, "You can't give a destination bucket without a source bucket. Please specify the --source-bucket option");
        std::process::exit(1);
    }

    let sync_start = std::time::Instant::now();

    let buckets_to_migrate = if let Some(bucket) = source_bucket.as_ref() {
        event!(Level::INFO, "Only bucket {} will be migrated", bucket);
        vec![bucket.clone()]
    } else {
        event!(
            Level::INFO,
            "All buckets of this Cellar add-ons will be migrated"
        );
        let riak_client = RiakCS::new(
            source_endpoint.clone(),
            source_access_key.clone(),
            source_secret_key.clone(),
            None,
        );

        let riak_buckets = riak_client.list_buckets().await?;
        riak_buckets
            .iter()
            .map(|bucket| bucket.name.clone())
            .collect()
    };

    // First make sure the destination buckets exist / can be created
    // If not, exit now
    if migrate::create_destination_buckets(
        destination_endpoint.clone(),
        destination_access_key.clone(),
        destination_secret_key.clone(),
        destination_bucket.clone(),
        destination_bucket_prefix.clone(),
        &buckets_to_migrate,
        dry_run,
    )
    .await
    .is_err()
    {
        event!(
            Level::ERROR,
            "Error while creating destination buckets. Aborting now."
        );
        std::process::exit(1);
    }

    let mut migration_results = Vec::with_capacity(buckets_to_migrate.len());

    for bucket in &buckets_to_migrate {
        if dry_run {
            event!(
                Level::INFO,
                "DRY-RUN | Bucket {} | Starting listing of files that need to be synchronized",
                bucket
            );
        } else {
            event!(
                Level::INFO,
                "Bucket {} | Starting migration of bucket",
                bucket
            );
        }

        let destination_bucket = if source_bucket.is_some() {
            if buckets_to_migrate.len() == 1 {
                destination_bucket.as_ref().unwrap_or(bucket)
            } else {
                panic!(
                    "We can't have a source bucket specified but with multiple buckets to migrate"
                );
            }
        } else {
            bucket
        };

        event!(
            Level::DEBUG,
            "Bucket {} | Starting synchronization of bucket with destination bucket {}",
            bucket,
            destination_bucket
        );

        let bucket_migration = BucketMigrationConfiguration {
            source_bucket: bucket.clone(),
            source_access_key: source_access_key.clone(),
            source_secret_key: source_secret_key.clone(),
            source_endpoint: source_endpoint.clone(),
            destination_bucket: format!("{}{}", destination_bucket_prefix, destination_bucket),
            destination_access_key: destination_access_key.clone(),
            destination_secret_key: destination_secret_key.clone(),
            destination_endpoint: destination_endpoint.clone(),
            delete_destination_files,
            max_keys,
            chunk_size: multipart_upload_chunk_size,
            sync_threads,
            dry_run,
        };

        event!(
            Level::TRACE,
            "Bucket {} | Bucket Migration Configuration: {:#?}",
            bucket,
            bucket_migration
        );

        let migration_result = migrate::migrate_bucket(bucket_migration).await;

        event!(
            Level::TRACE,
            "Bucket {} | Migration result: {:#?}",
            bucket,
            migration_result
        );

        if !dry_run {
            event!(
                Level::INFO,
                "Bucket {} | Bucket has been synchronized",
                bucket
            );
        }

        migration_results.push(migration_result);
    }

    if dry_run {
        let all_stats = migration_results
            .iter()
            .map(|result| match result {
                Ok(stats) => Some(stats),
                Err(error) => error
                    .downcast_ref::<BucketMigrationError>()
                    .map(|err| &err.stats),
            })
            .flatten()
            .collect::<Vec<&BucketMigrationStats>>();

        let all_objects = all_stats
            .iter()
            .map(|stat| &stat.objects)
            .flatten()
            .collect::<Vec<&ObjectContents>>();

        let all_objects_to_delete = all_stats
            .iter()
            .map(|stat| &stat.objects_to_delete)
            .flatten()
            .collect::<Vec<&rusoto_s3::Object>>();

        event!(
            Level::INFO,
            "Those objects need to be sync: {:#?}",
            all_stats
                .iter()
                .map(|stats| {
                    stats.objects.iter().map(|object| {
                        format!(
                            "{}/{} - {}",
                            stats.bucket,
                            object.get_key(),
                            ByteSize(object.get_size())
                        )
                    })
                })
                .flatten()
                .collect::<Vec<String>>()
        );

        event!(Level::TRACE, "Objects to sync: {:#?}", all_objects);

        if delete_destination_files {
            event!(
                Level::INFO,
                "Those objects will be deleted on the destination bucket because they are not on the source bucket: {:#?}",
                all_stats
                    .iter()
                    .map(|stats| {
                        stats.objects_to_delete.iter().map(|object| {
                            format!(
                                "{}/{} - {}",
                                stats.bucket,
                                object.key.as_ref().unwrap(),
                                ByteSize(object.size.unwrap_or(0) as u64)
                            )
                        })
                    })
                    .flatten()
                    .collect::<Vec<String>>()
            );
            event!(
                Level::TRACE,
                "Objects to delete: {:#?}",
                all_objects_to_delete
            );
        }

        let total_sync_bytes = all_objects
            .iter()
            .fold(0, |acc, object| acc + object.get_size() as u64);

        event!(
            Level::INFO,
            "Total files to sync: {} for a total of {}",
            all_objects.len(),
            ByteSize(total_sync_bytes)
        );

        if delete_destination_files {
            let total_delete_bytes = all_objects_to_delete
                .iter()
                .fold(0, |acc, object| acc + object.size.unwrap_or(0) as u64);

            event!(
                Level::INFO,
                "Total files to delete: {} for a total of {}",
                all_objects_to_delete.len(),
                ByteSize(total_delete_bytes)
            );
        }
    }

    let elapsed = sync_start.elapsed();

    for (index, migration_result) in migration_results.iter().enumerate() {
        let bucket = buckets_to_migrate
            .get(index)
            .expect("Bucket should be at index");

        if let Err(error) = migration_result {
            if let Some(err) = error.downcast_ref::<BucketMigrationError>() {
                for f in &err.errors {
                    event!(Level::ERROR, "Bucket {} | {}", bucket, f);
                }
            } else {
                event!(
                    Level::ERROR,
                    "Bucket {} | Error during synchronization: {:#?}",
                    bucket,
                    error
                );
            }
        }
    }

    let synchronization_size = migration_results.iter().fold(0, |acc, migration_result| {
        let stats = match migration_result {
            Ok(stats) => Some(stats),
            Err(error) => error
                .downcast_ref::<BucketMigrationError>()
                .map(|error| &error.stats),
        };

        if let Some(bucket_stats) = stats {
            acc + bucket_stats.synchronization_size
        } else {
            acc
        }
    });

    event!(
        Level::INFO,
        "Sync took {:?} for {} ({}/s)",
        elapsed,
        ByteSize(synchronization_size as u64),
        ByteSize((synchronization_size as f64 / elapsed.as_secs_f64()) as u64)
    );

    Ok(())
}
