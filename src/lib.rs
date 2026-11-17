#![allow(clippy::permissions_set_readonly_false)]

mod config;
mod status;

use anyhow::bail;
use config::{AVERAGE_DAYS_TO_HASH_EVERYTHING, REPO_FLAGS, ignore, protect};
use git2::Repository;
use rand::random;
use serde::{Deserialize, Serialize};
use status::StatusUpdate;
use std::error::Error;
use std::fs;
use std::fs::{File, read_to_string};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::thread::sleep;
use std::time::Duration;
use std::time::SystemTime;
use time::format_description;
use uuid::Uuid;
use xxhash_rust::xxh3::xxh3_64;

use crate::status::update_status;

#[derive(Debug, Deserialize, Serialize)]
struct SerializableBackupData {
    origin: String,
    destination: String,
    last_backup: SystemTime,
    files: usize,
    folders: usize,
    dump_secret: Uuid,
}

#[derive(Clone)]
struct BackupData {
    origin: PathBuf,
    destination: PathBuf,
    last_backup: SystemTime,
    files: usize,
    folders: usize,
    dump_secret: Uuid,
}

pub struct BackupResult {
    total_files: usize,

    added_files: Vec<String>,
    modified_files: Vec<String>,
    deleted_files: Vec<String>,
    failed_files: Vec<String>,
    time_taken: Duration,
    backup_time: SystemTime,
}

//TODO: Write test script

fn initialise_backup(origin: &str, destination: &str) {
    let mut destination_path = PathBuf::from(destination);
    let origin_path = PathBuf::from(origin);

    if fs::exists(&destination_path).expect("Should be able to access destination!") {
        panic!("Cannot overwrite destination if doing full backup!");
    }
    if !fs::exists(&origin_path).expect("Should be able to access origin!") {
        panic!("Cannot find folder to backup!");
    }

    fs::create_dir(&destination_path).expect("Need permission to create destination!");

    destination_path.push(".backup");

    fs::create_dir(&destination_path).expect("Need permission to create backup folder!");

    let uuid_path = origin_path.join(".backup_uuids.txt");
    allow_write(&uuid_path).unwrap_or_default();
    let uuid = Uuid::new_v4();
    let uuids = fs::read_to_string(&uuid_path).unwrap_or_default() + format!("{}\n", uuid).as_str();
    let mut origin_secrets =
        File::create(&uuid_path).expect("Should be allowed to create secret file in origin!");
    origin_secrets
        .write_all(uuids.as_bytes())
        .expect("Should be able to write to origin secret file!");
    drop(origin_secrets);
    make_read_only(&uuid_path)
        .expect("Should be able to modify permissions on origin secret file!");

    let serialise_backup_data = SerializableBackupData {
        origin: String::from(origin),
        destination: String::from(destination),
        last_backup: SystemTime::UNIX_EPOCH,
        files: 0,
        folders: 1,
        dump_secret: uuid,
    };

    //TODO: Put and check version number into backup data
    let destination_backup_log = PathBuf::from(destination).join(".backup/backup.json");
    let mut file = File::create(&destination_backup_log)
        .expect("Unexpected error occured in saving backup file");

    file.write_all(
        serde_json::to_string(&serialise_backup_data)
            .unwrap()
            .as_bytes(),
    )
    .expect("Could not save backup data!");
}

fn get_uuids(origin_path: &Path) -> Vec<Uuid> {
    let uuid_path = origin_path.join(".backup_uuids.txt");

    read_to_string(uuid_path)
        .expect("Should be able to read origin secrets!")
        .split("\n")
        .collect::<Vec<&str>>()
        .iter()
        .filter(|input: &&&str| !input.is_empty())
        .map(|input: &&str| Uuid::parse_str(input).expect("Origin secrets should contain uuids"))
        .collect()
}

fn make_read_only(path: &PathBuf) -> Result<(), anyhow::Error> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_readonly(true);
    fs::set_permissions(path, perms).expect("Need permission to edit destination!");
    Ok(())
}

fn allow_write(path: &PathBuf) -> Result<(), anyhow::Error> {
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_readonly(false);
    fs::set_permissions(path, perms).expect("Need permission to edit destination!");
    Ok(())
}

fn copy_file(
    backup_data: &BackupData,
    relative_path: &PathBuf,
    tx: &mpsc::Sender<StatusUpdate>,
    already_existed: bool,
) -> Result<(), anyhow::Error> {
    let origin_path = backup_data.origin.join(relative_path);
    let destination_path = backup_data.destination.join(relative_path);

    if already_existed {
        allow_write(&destination_path)?;
    }
    fs::copy(origin_path, &destination_path)?;
    make_read_only(&destination_path)?;

    if already_existed {
        tx.send(StatusUpdate::FileModified(
            relative_path.clone().into_string().unwrap(),
        ))
        .unwrap_or_else(|e| {
            dbg!(e);
        });
    } else {
        tx.send(StatusUpdate::FileAdded(
            relative_path.clone().into_string().unwrap(),
        ))
        .unwrap_or_else(|e| {
            dbg!(e);
        });
    }
    Ok(())
}

fn compute_hash(path: PathBuf) -> Result<u64, io::Error> {
    let mut file = File::open(path)?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(xxh3_64(&buf))
}

fn count_contents(folder_path: &PathBuf) -> Result<(i32, i32), anyhow::Error> {
    let entries = fs::read_dir(folder_path)?;
    let (mut inside_files, mut inside_folders) = (0, 0);
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            let entry_path = entry.path();
            inside_folders += 1;
            let (sub_folders, sub_files) = count_contents(&entry_path)?;
            (inside_folders, inside_files) =
                (inside_folders + sub_folders, inside_files + sub_files);
        } else {
            inside_files += 1;
        }
    }
    Ok((inside_folders, inside_files))
}

fn check_file(
    backup_data: &BackupData,
    relative_path: &PathBuf,
    random_check_probability: f32,
) -> Result<(bool, bool), Box<dyn Error>> {
    let origin_path = backup_data.origin.join(relative_path);
    let destination_path = backup_data.destination.join(relative_path);

    let io::Result::Ok(dest_file) = fs::metadata(&destination_path) else {
        return Ok((true, false));
    };

    let orig_file = fs::metadata(&origin_path)?;
    if orig_file.modified()? >= dest_file.modified()?
        || orig_file.modified()? >= backup_data.last_backup
    {
        //TODO: remove clones below
        let hash_mismatch =
            compute_hash(origin_path.clone())? != compute_hash(destination_path.clone())?;
        return Ok((hash_mismatch, true));
    }

    //TODO: Check random_check_probability works as intended
    if random::<f32>() <= random_check_probability {
        //TODO: This hash computation is in the wrong thread
        let hash_mismatch = compute_hash(origin_path)? != compute_hash(destination_path)?;
        return Ok((hash_mismatch, true));
    }
    Ok((false, false))
}

fn get_git_repo(path: &PathBuf) -> Option<Repository> {
    Repository::open_ext(path, REPO_FLAGS, &[] as &[&std::ffi::OsStr]).ok()
}

fn backup_folder(
    backup_data: &BackupData,
    relative_path: PathBuf,
    backup_tx: &mpsc::Sender<PathBuf>,
    status_tx: &mpsc::Sender<StatusUpdate>,
    repo: &Option<Repository>,
    mut logging_open: bool,
) -> bool {
    let origin_path = backup_data.origin.join(&relative_path);
    let destination_path = backup_data.destination.join(&relative_path);

    let repo = if repo.is_none() {
        &get_git_repo(&origin_path)
    } else {
        repo
    };

    let mut files_to_copy: Vec<PathBuf> = Vec::new();
    let mut files_to_remove: Vec<PathBuf> = Vec::new();
    let mut folders_to_copy: Vec<PathBuf> = Vec::new();
    let mut folders_to_remove: Vec<PathBuf> = Vec::new();

    let entries = match fs::read_dir(&origin_path) {
        Ok(entries) => entries,
        Err(e) => {
            dbg!(e);

            if logging_open {
                status_tx
                    .send(StatusUpdate::FolderFailed(
                        relative_path.into_string().unwrap(),
                    ))
                    .unwrap_or_else(|e| {
                        dbg!(e);
                        logging_open = false;
                    });
            }
            return logging_open;
        }
    };

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                dbg!(e);
                if logging_open {
                    status_tx
                        .send(StatusUpdate::FolderFailed(
                            relative_path.clone().into_string().unwrap(),
                        ))
                        .unwrap_or_else(|e| {
                            dbg!(e);
                            logging_open = false;
                        });
                }
                continue;
            }
        };

        let entry_path = relative_path.join(entry.file_name());

        let absolute_path = origin_path.join(entry.file_name());

        let should_ignore = match ignore(&absolute_path, repo) {
            Ok(res) => res,
            Err(e) => {
                dbg!(e);
                if logging_open {
                    status_tx
                        .send(StatusUpdate::FolderFailed(
                            entry_path.into_string().unwrap(),
                        ))
                        .unwrap_or_else(|e| {
                            dbg!(e);
                            logging_open = false;
                        });
                }
                continue;
            }
        };

        if should_ignore {
            continue;
        }

        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(e) => {
                dbg!(e);
                if logging_open {
                    status_tx
                        .send(StatusUpdate::FolderFailed(
                            entry_path.into_string().unwrap(),
                        ))
                        .unwrap_or_else(|e| {
                            dbg!(e);
                            logging_open = false;
                        });
                }
                continue;
            }
        };
        if file_type.is_dir() {
            folders_to_copy.push(entry_path);
        } else {
            files_to_copy.push(entry_path);
        }
    }
    // TODO: Ensure that if a file becomes a folder or vice versa it's ok

    let entries =
        fs::read_dir(&destination_path).expect("Should be able to read destination directories!");
    let mut existing_folders: i32 = 0;
    let mut existing_files: i32 = 0;
    for entry in entries {
        let entry = entry.expect("Should be able to read destination directory entries!");
        let entry_path = relative_path.join(entry.file_name());
        if protect(&entry_path) {
            continue;
        }
        if entry
            .file_type()
            .expect("Should be able to determine file type in destination!")
            .is_dir()
        {
            if !folders_to_copy.contains(&entry_path) {
                folders_to_remove.push(entry_path);
            }
            existing_folders += 1;
        } else {
            if !files_to_copy.contains(&entry_path) {
                files_to_remove.push(entry_path);
            }
            existing_files += 1;
        }
    }

    let delta_folders = folders_to_copy.len() as i32 - existing_folders;
    let delta_files = files_to_copy.len() as i32 - existing_files;

    if logging_open {
        status_tx
            .send(StatusUpdate::FolderDiscovered(delta_folders, delta_files))
            .unwrap_or_else(|e| {
                dbg!(e);
                logging_open = false;
            });
    }

    for file in files_to_remove {
        fs::remove_file(backup_data.destination.join(&file))
            .expect("Should have access to destination!");
        if logging_open {
            status_tx
                .send(StatusUpdate::FileDeleted(file.into_string().unwrap()))
                .unwrap_or_else(|e| {
                    dbg!(e);
                    logging_open = false;
                });
        }
    }
    for folder in folders_to_remove {
        let (folders_inside, files_inside) =
            count_contents(&destination_path.join(&folder)).unwrap_or_default();
        fs::remove_dir_all(backup_data.destination.join(&folder))
            .expect("Should have access to destination!");
        if logging_open {
            status_tx
                .send(StatusUpdate::FolderDeleted(
                    folder.into_string().unwrap(),
                    folders_inside,
                    files_inside,
                ))
                .unwrap_or_else(|e| {
                    dbg!(e);
                    logging_open = false;
                });
        }
    }

    if logging_open {
        status_tx
            .send(StatusUpdate::FolderScanned())
            .unwrap_or_else(|e| {
                dbg!(e);
                logging_open = false;
            });
    }
    for file in files_to_copy {
        if logging_open {
            backup_tx.send(file).unwrap_or_else(|e| {
                dbg!(e);
                logging_open = false;
            });
        }
    }
    for folder in folders_to_copy {
        match fs::create_dir_all(backup_data.destination.join(&folder)) {
            Err(e) => {
                dbg!(e);
                if logging_open {
                    status_tx
                        .send(StatusUpdate::FolderFailed(folder.into_string().unwrap()))
                        .unwrap_or_else(|e| {
                            dbg!(e);
                            logging_open = false;
                        });
                }
            }
            Ok(()) => {
                logging_open = backup_folder(
                    backup_data,
                    folder,
                    backup_tx,
                    status_tx,
                    repo,
                    logging_open,
                )
            }
        }
    }
    logging_open
}

fn compute_random_check_probability(last_backup: SystemTime, current_time: SystemTime) -> f32 {
    current_time
        .duration_since(last_backup)
        .expect("Time should have already passed!")
        .as_secs_f32()
        / Duration::new(60 * 60 * 24 * AVERAGE_DAYS_TO_HASH_EVERYTHING, 0).as_secs_f32()
}

fn write_result(res: &BackupResult, mut file: File) -> Result<(), io::Error> {
    writeln!(
        file,
        "Backup on: {}",
        convert_datetime_to_string(res.backup_time)
    )?;
    writeln!(file, "Duration: {:?}", res.time_taken)?;
    writeln!(file, "Total files: {}", res.total_files)?;

    writeln!(file, "\nFILES ADDED: {}", res.added_files.len())?;
    for filename in &res.added_files {
        writeln!(file, "{}", filename)?;
    }
    writeln!(file, "\nFILES MODIFIED: {}", res.modified_files.len())?;
    for filename in &res.modified_files {
        writeln!(file, "{}", filename)?;
    }
    writeln!(file, "\nFILES DELETED: {}", res.deleted_files.len())?;
    for filename in &res.deleted_files {
        writeln!(file, "{}", filename)?;
    }
    writeln!(file, "\nFAILED TO BACKUP: {}", res.failed_files.len())?;
    for filename in &res.failed_files {
        writeln!(file, "{}", filename)?;
    }
    Ok(())
}

fn convert_datetime_to_string(sys_time: SystemTime) -> String {
    let dt: time::OffsetDateTime = sys_time.into();
    let format =
        format_description::parse_borrowed::<3>("[year]-[month]-[day] [hour]:[minute]:[second]")
            .unwrap();
    dt.format(&format).unwrap()
}

pub fn attempt_update_backup(
    destination: &str,
    logging: bool,
) -> Result<BackupResult, anyhow::Error> {
    let program_start_time: SystemTime = SystemTime::now();
    sleep(Duration::new(1, 0));
    let destination_backup_log = PathBuf::from(destination).join(".backup/backup.json");

    let Ok(mut file) = File::open(&destination_backup_log) else {
        bail!("Could not locate a destination backup dump");
    };

    let mut backup_data_str = String::new();
    file.read_to_string(&mut backup_data_str)
        .expect("Should be able to read backup dump");

    let stored_data = serde_json::from_str::<SerializableBackupData>(&backup_data_str)
        .expect("Backup Dump Corrupted");
    let random_check_probability =
        compute_random_check_probability(stored_data.last_backup, program_start_time);
    let backup_data = BackupData {
        origin: PathBuf::from(stored_data.origin),
        destination: PathBuf::from(stored_data.destination),
        last_backup: stored_data.last_backup,
        files: stored_data.files,
        folders: stored_data.folders,
        dump_secret: stored_data.dump_secret,
    };

    //TODO Assert both are folders

    let secrets: Vec<Uuid> = get_uuids(&backup_data.origin);
    assert!(secrets.contains(&backup_data.dump_secret));

    let (status_tx, status_rx) = mpsc::channel::<StatusUpdate>();
    let (backup_tx, backup_rx) = mpsc::channel::<PathBuf>();

    let child_status_tx = status_tx.clone();
    let child_backup_data = backup_data.clone();
    let handle1 = thread::spawn(move || {
        let mut logging_open = true;
        for recv in backup_rx {
            let (must_be_copied, already_exists) =
                check_file(&child_backup_data, &recv, random_check_probability).unwrap_or_else(
                    |_| {
                        if logging_open {
                            child_status_tx
                                .send(StatusUpdate::FileFailed(
                                    recv.clone().into_string().unwrap(),
                                ))
                                .unwrap_or_else(|e| {
                                    dbg!(e);
                                    logging_open = false;
                                });
                        }
                        (false, false)
                    },
                );
            if must_be_copied {
                copy_file(&child_backup_data, &recv, &child_status_tx, already_exists)
                    .unwrap_or_else(|_| {
                        if logging_open {
                            child_status_tx
                                .send(StatusUpdate::FileFailed(recv.into_string().unwrap()))
                                .unwrap_or_else(|e| {
                                    dbg!(e);
                                    logging_open = false;
                                })
                        }
                    });
            } else {
                if logging_open {
                    child_status_tx
                        .send(StatusUpdate::FileUnchanged())
                        .unwrap_or_else(|e| {
                            logging_open = false;
                            dbg!(e);
                        });
                }
            }
        }
    });

    let child_backup_data = backup_data.clone();

    let handle2 = thread::spawn(move || {
        update_status(&child_backup_data, status_rx, program_start_time, logging)
    });

    backup_folder(
        &backup_data,
        PathBuf::new(),
        &backup_tx,
        &status_tx,
        &get_git_repo(&backup_data.origin),
        true,
    );

    status_tx
        .send(StatusUpdate::ScanFinished())
        .unwrap_or_else(|e| {
            dbg!(e);
        });

    drop(backup_tx);
    handle1.join().unwrap();
    drop(status_tx);
    handle2.join().unwrap()
}

pub fn attempt_full_backup(
    destination: &str,
    origin: &str,
    logging: bool,
) -> Result<BackupResult, anyhow::Error> {
    initialise_backup(origin, destination);
    attempt_update_backup(destination, logging)
}
