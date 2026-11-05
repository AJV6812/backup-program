use serde::{Deserialize, Serialize};
use std::collections::LinkedList;
use std::env;
use std::fs;
use std::fs::File;
use std::io;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::SystemTime;
use time::format_description;
use uuid::Uuid;

#[derive(Debug, Deserialize, Serialize)]
struct SerializableBackupData {
    origin: String,
    destination: String,
    last_backup: SystemTime,
    files: usize,
    dump_secret: Uuid,
}

#[derive(Clone)]
struct BackupData {
    origin: PathBuf,
    destination: PathBuf,
    last_backup: SystemTime,
    files: usize,
    dump_secret: Uuid,
}

struct BackupResult {
    total_files: usize,

    added_files: LinkedList<String>,
    modified_files: LinkedList<String>,
    deleted_files: LinkedList<String>,
    failed_files: LinkedList<String>,
    time_taken: Duration,
    backup_time: SystemTime,
}

enum StatusUpdate {
    FileAdded(String),
    FileModified(String),
    FileDeleted(String),
    FileFailed(String),
    FolderDiscovered(usize),
    FileUnchanged(),
    FileDiscovered(),
    //TODO: Ensure folder discovered and file unchanged are used
    //TODO: May need to handle receiver close and error conditions separately.
}

fn initialise_backup(origin: &str, destination: &str) {
    let mut destination_path = PathBuf::from(destination);
    let origin_path = PathBuf::from(origin);

    //TODO: No assert or unwrap or expect anywhere
    assert!(!fs::exists(&destination_path).unwrap());
    assert!(fs::exists(origin_path).unwrap());

    fs::create_dir(&destination_path).expect("Need permission to create destination!");

    destination_path.push(".backup");

    fs::create_dir(&destination_path).expect("Need permission to create backup folder!");

    destination_path.push("backup.bin");
    fs::create_dir(destination_path).expect("Need permission to create backup dump!");

    //TODO: Write secret to origin
}

fn copy_file(
    backup_data: &BackupData,
    relative_path: &PathBuf,
    tx: &mpsc::Sender<StatusUpdate>,
    already_existed: bool,
) {
    let origin_path = backup_data.origin.join(relative_path);
    let destination_path = backup_data.destination.join(relative_path);

    fs::copy(origin_path, &destination_path).unwrap();

    let mut perms = fs::metadata(&destination_path).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(destination_path, perms).expect("Need permission to edit destination!");

    if already_existed {
        tx.send(StatusUpdate::FileModified(String::from(
            relative_path.to_str().unwrap(),
        )))
        .unwrap_or_else(|_| println!("Logging closed!"));
    } else {
        tx.send(StatusUpdate::FileAdded(String::from(
            relative_path.to_str().unwrap(),
        )))
        .unwrap_or_else(|_| println!("Logging closed!"));
    }
}

fn check_file(
    backup_data: &BackupData,
    relative_path: &PathBuf,
    tx: &mpsc::Sender<StatusUpdate>,
    folder_discovered: bool,
) -> (bool, bool) {
    let origin_path = backup_data.origin.join(relative_path);
    let destination_path = backup_data.destination.join(relative_path);

    //TODO: Need to implement ignore and preserve functions

    let orig_file = fs::metadata(&origin_path).unwrap();
    let io::Result::Ok(dest_file) = fs::metadata(&destination_path) else {
        if folder_discovered {
            tx.send(StatusUpdate::FileDiscovered())
                .unwrap_or_else(|_| println!("Logging closed!"));
        }
        return (true, false);
    };

    //TODO: Need to do random hash checks as well
    //TODO: remove unwraps that are below
    (
        orig_file.modified().unwrap() >= dest_file.created().unwrap(),
        true,
    )
}

fn backup_folder(
    backup_data: &BackupData,
    relative_path: PathBuf,
    backup_tx: &mpsc::Sender<PathBuf>,
    status_tx: &mpsc::Sender<StatusUpdate>,
) {
    //TODO: Use progress_tx
    let origin_path = backup_data.origin.join(&relative_path);
    let destination_path = backup_data.destination.join(&relative_path);
    let entries: Vec<Result<fs::DirEntry, io::Error>> =
        fs::read_dir(origin_path).unwrap().collect();
    //TODO: Remove above unwrap
    if !fs::exists(&destination_path).unwrap() {
        fs::create_dir(&destination_path).expect("Need to be able to create files in destination!");
        status_tx
            .send(StatusUpdate::FolderDiscovered(entries.len()))
            .unwrap_or_else(|_| println!("No status terminal!"));
    };
    for entry in entries {
        //TODO: Above unwrap and expect below should eventually be removed
        let entry = entry.unwrap();

        if entry.file_type().unwrap().is_dir() {
            backup_folder(
                backup_data,
                relative_path.join(entry.file_name()),
                backup_tx,
                status_tx,
            );
        } else {
            //TODO: Possibly remove unwrap
            backup_tx
                .send(relative_path.join(entry.file_name()))
                .unwrap();
        }
    }
    // TODO: Delete all other files in this folder
}

fn main() {
    let program_start_time = SystemTime::now();

    let args: Vec<String> = env::args().collect();

    let destination = args.get(1).expect("Error, no backup dump provided!");

    let mut destination_backup_log = destination.clone();
    destination_backup_log.push_str("/.backup/backup.json");

    let file = File::open(&destination_backup_log);

    let backup_data: BackupData = match file {
        Result::Ok(mut read_file) => {
            let mut backup_data_str = String::new();
            read_file
                .read_to_string(&mut backup_data_str)
                .expect("Should be able to read backup dump");

            let stored_data: SerializableBackupData =
                serde_json::from_str(&backup_data_str).unwrap();
            BackupData {
                origin: PathBuf::from(stored_data.origin),
                destination: PathBuf::from(stored_data.destination),
                last_backup: stored_data.last_backup,
                files: stored_data.files,
                dump_secret: stored_data.dump_secret,
            }
        }
        Result::Err(error) => {
            println!("Attempting full backup due to {error}");
            let origin = args
                .get(2)
                .expect("Error backup data does not exist and no origin in file!");
            initialise_backup(origin, destination);
            BackupData {
                origin: PathBuf::from(origin),
                destination: PathBuf::from(destination),
                last_backup: SystemTime::UNIX_EPOCH,
                files: 0,
                dump_secret: Uuid::new_v4(),
            }
        }
    };

    //TODO Assert secret is the same
    //TODO Assert both are folders

    let (status_tx, status_rx) = mpsc::channel::<StatusUpdate>();
    let (backup_tx, backup_rx) = mpsc::channel::<PathBuf>();

    let child_status_tx = status_tx.clone();
    let child_backup_data = backup_data.clone();
    let handle1 = thread::spawn(move || {
        for recv in backup_rx {
            let (must_be_copied, already_exists) = check_file(
                &child_backup_data,
                &recv,
                &child_status_tx,
                true, //TODO: Fix above to not be literal
            );
            if must_be_copied {
                copy_file(&child_backup_data, &recv, &child_status_tx, already_exists);
            }
        }
    });

    let child_backup_data = backup_data.clone();
    let handle2 =
        thread::spawn(move || update_status(&child_backup_data, status_rx, program_start_time));

    backup_folder(&backup_data, PathBuf::new(), &backup_tx, &status_tx);

    handle2.join().unwrap();
    drop(status_tx);
    handle1.join().unwrap();
}

fn update_status(
    backup_data: &BackupData,
    status_rx: mpsc::Receiver<StatusUpdate>,
    program_start_time: SystemTime,
) {
    let mut backup_data = backup_data.clone();
    let mut total_files = backup_data.files;
    let mut scanned_files = 0;
    let destination_backup_log = backup_data.destination.clone().join(".backup/backup.json");
    let (mut added_files, mut modified_files, mut deleted_files, mut failed_files) = (
        LinkedList::new(),
        LinkedList::new(),
        LinkedList::new(),
        LinkedList::new(),
    );

    for recv in status_rx {
        match recv {
            StatusUpdate::FileAdded(str) => {
                added_files.push_back(str);
                scanned_files += 1
            }
            StatusUpdate::FileFailed(str) => {
                failed_files.push_back(str);
                scanned_files += 1
            }
            StatusUpdate::FileDeleted(str) => {
                deleted_files.push_back(str);
                total_files -= 1
            }
            StatusUpdate::FileModified(str) => modified_files.push_back(str),
            StatusUpdate::FolderDiscovered(num) => total_files += num,
            StatusUpdate::FileDiscovered() => total_files += 1,
            StatusUpdate::FileUnchanged() => scanned_files += 1,
        }
        dbg!(scanned_files, total_files);
    }

    let backup_result = BackupResult {
        added_files,
        modified_files,
        deleted_files,
        failed_files,
        total_files,
        time_taken: SystemTime::now()
            .duration_since(program_start_time)
            .unwrap(),
        backup_time: program_start_time,
        //TODO: Remove above unwrap?
    };

    backup_data.last_backup = program_start_time;

    let mut destination_path_log = backup_data
        .destination
        .clone()
        .join(".backup".to_owned() + convert_datetime_to_string(backup_data.last_backup).as_str());
    destination_path_log.set_extension("log");

    let Result::Ok(path_log) = File::create(destination_path_log) else {
        panic!("Cannot create a log file in destination!");
    };

    //TODO: Determine how to write backup_result

    write_result(backup_result, path_log).expect("Should be able to write to path location!");

    let mut file = File::open(&destination_backup_log)
        .expect("Unexpected error occured in saving backup file");

    let serialise_backup_data = SerializableBackupData {
        origin: String::from(backup_data.origin.to_str().unwrap()),
        destination: String::from(backup_data.destination.to_str().unwrap()),
        last_backup: backup_data.last_backup,
        files: backup_data.files,
        dump_secret: backup_data.dump_secret,
    };

    file.write_all(
        serde_json::to_string(&serialise_backup_data)
            .unwrap()
            .as_bytes(),
    )
    .expect("Could not save backup data!");
}

fn write_result(res: BackupResult, mut file: File) -> Result<(), io::Error> {
    writeln!(
        file,
        "Backup on: {}",
        convert_datetime_to_string(res.backup_time)
    )?;
    writeln!(file, "Duration: {:?}", res.time_taken)?;
    writeln!(file, "Total file: {}", res.total_files)?;

    writeln!(file, "\nFILES ADDED: {}", res.added_files.len())?;
    for filename in res.added_files {
        writeln!(file, "{}", filename)?;
    }
    writeln!(file, "\nFILES MODIFIED: {}", res.modified_files.len())?;
    for filename in res.modified_files {
        writeln!(file, "{}", filename)?;
    }
    writeln!(file, "\nFILES DELETED: {}", res.deleted_files.len())?;
    for filename in res.deleted_files {
        writeln!(file, "{}", filename)?;
    }
    writeln!(file, "\nFAILED TO BACKUP: {}", res.failed_files.len())?;
    for filename in res.failed_files {
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
