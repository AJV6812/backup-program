use anyhow::bail;
use crossterm::terminal;
use crossterm::{cursor::MoveUp, execute};
use git2::Repository;
use rand::random;
use serde::{Deserialize, Serialize};
use std::env;
use std::error::Error;
use std::fmt::Display;
use std::fs;
use std::fs::File;
use std::io;
use std::io::StdoutLock;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::thread::sleep;
use std::time::Duration;
use std::time::SystemTime;
use time::format_description;
use uuid::Uuid;
use xxhash_rust::xxh3::xxh3_64;

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

struct BackupResult {
    total_files: usize,

    added_files: Vec<String>,
    modified_files: Vec<String>,
    deleted_files: Vec<String>,
    failed_files: Vec<String>,
    time_taken: Duration,
    backup_time: SystemTime,
}

//TODO: Write test script

#[derive(Clone)]
enum StatusUpdate {
    FileAdded(String),
    FileModified(String),
    FileDeleted(String),
    FileFailed(String),
    FileUnchanged(),
    FolderDiscovered(i32, i32),
    FolderScanned(),
    FolderFailed(String),
    FolderDeleted(String),
}
impl StatusUpdate {
    fn should_format(&self) -> bool {
        match self {
            StatusUpdate::FileAdded(_) => true,
            StatusUpdate::FolderDeleted(_) => true,
            StatusUpdate::FolderFailed(_) => true,
            StatusUpdate::FileFailed(_) => true,
            StatusUpdate::FileDeleted(_) => true,
            StatusUpdate::FileModified(_) => true,
            StatusUpdate::FileUnchanged() => false,
            StatusUpdate::FolderScanned() => false,
            StatusUpdate::FolderDiscovered(_, _) => false,
        }
    }
}
impl Display for StatusUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        match self {
            StatusUpdate::FileAdded(str) => write!(f, "ADDED: {str}"),
            StatusUpdate::FolderDeleted(str) => write!(f, "DELETED: {str}"),
            StatusUpdate::FolderFailed(str) => write!(f, "FAILED: {str}"),
            StatusUpdate::FileFailed(str) => write!(f, "FAILED: {str}"),
            StatusUpdate::FileDeleted(str) => write!(f, "DELETED: {str}"),
            StatusUpdate::FileModified(str) => write!(f, "OVERWRITTEN: {str}"),
            StatusUpdate::FileUnchanged() => Err(std::fmt::Error),
            StatusUpdate::FolderScanned() => Err(std::fmt::Error),
            StatusUpdate::FolderDiscovered(_, _) => Err(std::fmt::Error),
        }
    }
}

const REPO_FLAGS: git2::RepositoryOpenFlags = git2::RepositoryOpenFlags::NO_SEARCH;

const AVERAGE_DAYS_TO_HASH_EVERYTHING: u64 = 365;

fn ignore(absolute_path: &Path, repo: &Option<Repository>) -> Result<bool, anyhow::Error> {
    Ok(match repo {
        Some(repo) => repo.status_should_ignore(absolute_path)?,
        None => false,
    })
}

fn protect(relative_path: &Path) -> bool {
    relative_path.starts_with(".backup/")
}

fn initialise_backup(origin: &str, destination: &str) -> Uuid {
    let mut destination_path = PathBuf::from(destination);
    let origin_path = PathBuf::from(origin);

    if fs::exists(&destination_path).expect("Should be able to access destination!") {
        panic!("Cannot overwrite destination if doing full backup!");
    }
    if !fs::exists(origin_path).expect("Should be able to access origin!") {
        panic!("Cannot find folder to backup!");
    }

    fs::create_dir(&destination_path).expect("Need permission to create destination!");

    destination_path.push(".backup");

    fs::create_dir(&destination_path).expect("Need permission to create backup folder!");

    let uuid_path = destination_path.join(".backup_uuids.txt");
    allow_write(&uuid_path).expect("Should be able to modify permissions on origin secrets file!");
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
    uuid
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
        .unwrap_or_else(|_| println!("Logging closed!"));
    } else {
        tx.send(StatusUpdate::FileAdded(
            relative_path.clone().into_string().unwrap(),
        ))
        .unwrap_or_else(|_| println!("Logging closed!"));
    }
    Ok(())
}

fn compute_hash(path: PathBuf) -> Result<u64, io::Error> {
    let mut file = File::open(path)?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(xxh3_64(&buf))
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

    if random::<f32>() <= random_check_probability {
        //TODO: This hash computation is in the wrong thread
        let hash_mismatch = compute_hash(origin_path)? != compute_hash(destination_path)?;
        if hash_mismatch {
            dbg!("Hash mismatch on {:?}", relative_path);
        }
        return Ok((hash_mismatch, true));
    }

    let orig_file = fs::metadata(&origin_path)?;
    Ok((orig_file.modified()? >= dest_file.modified()?, true))
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
) {
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

    let Ok(entries) = fs::read_dir(&origin_path) else {
        status_tx
            .send(StatusUpdate::FolderFailed(
                relative_path.into_string().unwrap(),
            ))
            .unwrap_or_else(|_| println!("Logging closed!"));
        return;
    };

    for entry in entries {
        let Ok(entry) = entry else {
            status_tx
                .send(StatusUpdate::FolderFailed(
                    relative_path.clone().into_string().unwrap(),
                ))
                .unwrap_or_else(|_| println!("Logging closed!"));
            continue;
        };

        let entry_path = relative_path.join(entry.file_name());
        let absolute_path = origin_path.join(entry.file_name());
        let Ok(should_ignore) = ignore(&absolute_path, repo) else {
            status_tx
                .send(StatusUpdate::FolderFailed(
                    entry_path.into_string().unwrap(),
                ))
                .unwrap_or_else(|_| {
                    println!("Logging closed!");
                });
            continue;
        };
        if should_ignore {
            continue;
        }

        let Ok(file_type) = entry.file_type() else {
            status_tx
                .send(StatusUpdate::FolderFailed(
                    entry_path.into_string().unwrap(),
                ))
                .unwrap_or_else(|_| println!("Logging closed!"));
            continue;
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

    status_tx
        .send(StatusUpdate::FolderDiscovered(delta_folders, delta_files))
        .unwrap_or_else(|_| println!("Logging closed!"));

    for file in files_to_remove {
        fs::remove_file(backup_data.destination.join(&file))
            .expect("Should have access to destination!");
        status_tx
            .send(StatusUpdate::FileDeleted(file.into_string().unwrap()))
            .unwrap_or_else(|_| println!("Logging closed!"));
    }
    for folder in folders_to_remove {
        fs::remove_dir_all(backup_data.destination.join(&folder))
            .expect("Should have access to destination!");
        status_tx
            .send(StatusUpdate::FolderDeleted(folder.into_string().unwrap()))
            .unwrap_or_else(|_| println!("Logging closed!"));
    }
    status_tx
        .send(StatusUpdate::FolderScanned())
        .unwrap_or_else(|_| println!("Logging closed!"));
    for file in files_to_copy {
        backup_tx
            .send(file)
            .unwrap_or_else(|_| println!("Logging closed!"));
    }
    for folder in folders_to_copy {
        if fs::create_dir(backup_data.destination.join(&folder)).is_err() {
            status_tx
                .send(StatusUpdate::FolderFailed(folder.into_string().unwrap()))
                .unwrap_or_else(|_| println!("Logging closed!"));
        }
    }
}

fn compute_random_check_probability(last_backup: SystemTime, current_time: SystemTime) -> f32 {
    current_time
        .duration_since(last_backup)
        .expect("Time should have already passed!")
        .as_secs_f32()
        / Duration::new(60 * 60 * 24 * AVERAGE_DAYS_TO_HASH_EVERYTHING, 0).as_secs_f32()
}

fn request_full_backup(
    args: &[String],
    destination: &str,
    error: String,
) -> Result<BackupData, anyhow::Error> {
    println!("Could not load previous backup due to {error}!");
    println!("Backup from scratch (Y/N)? ");
    let mut res = String::new();
    io::stdin().read_line(&mut res)?;
    if !res.to_lowercase().contains("y") {
        bail!("Expecting yes");
    }

    let origin = args
        .get(2)
        .expect("Error backup data does not exist and no origin in args!");
    let uuid = initialise_backup(origin, destination);
    Ok(BackupData {
        origin: PathBuf::from(origin),
        destination: PathBuf::from(destination),
        last_backup: SystemTime::UNIX_EPOCH,
        files: 0,
        folders: 0,
        dump_secret: uuid,
    })
}

fn main() {
    let program_start_time = SystemTime::now();

    sleep(Duration::new(1, 0));

    let args: Vec<String> = env::args().collect();

    let destination = args.get(1).expect("Error, no backup dump provided!");

    let mut destination_backup_log = destination.clone();
    destination_backup_log.push_str("/.backup/backup.json");

    let file = File::open(&destination_backup_log);

    let mut random_check_probability: f32 = 0.0;

    let backup_data: BackupData = match file {
        Result::Ok(mut read_file) => {
            let mut backup_data_str = String::new();
            read_file
                .read_to_string(&mut backup_data_str)
                .expect("Should be able to read backup dump");

            match serde_json::from_str::<SerializableBackupData>(&backup_data_str) {
                Ok(stored_data) => {
                    random_check_probability = compute_random_check_probability(
                        stored_data.last_backup,
                        program_start_time,
                    );
                    BackupData {
                        origin: PathBuf::from(stored_data.origin),
                        destination: PathBuf::from(stored_data.destination),
                        last_backup: stored_data.last_backup,
                        files: stored_data.files,
                        folders: stored_data.folders,
                        dump_secret: stored_data.dump_secret,
                    }
                }
                Err(e) => {
                    request_full_backup(&args, destination, e.to_string()).expect("Backup failed.")
                }
            }
        }
        Result::Err(e) => {
            request_full_backup(&args, destination, e.to_string()).expect("Backup failed")
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
            let (must_be_copied, already_exists) =
                check_file(&child_backup_data, &recv, random_check_probability).unwrap_or_else(
                    |_| {
                        child_status_tx
                            .send(StatusUpdate::FileFailed(
                                recv.clone().into_string().unwrap(),
                            ))
                            .unwrap_or_else(|_| println!("Logging closed!"));
                        (false, false)
                    },
                );
            if must_be_copied {
                copy_file(&child_backup_data, &recv, &child_status_tx, already_exists)
                    .unwrap_or_else(|_| {
                        child_status_tx
                            .send(StatusUpdate::FileFailed(recv.into_string().unwrap()))
                            .unwrap_or_else(|_| println!("Logging closed!"))
                    });
            } else {
                child_status_tx
                    .send(StatusUpdate::FileUnchanged())
                    .unwrap_or_else(|_| println!("Logging closed!"));
            }
        }
    });

    let child_backup_data = backup_data.clone();
    let handle2 =
        thread::spawn(move || update_status(&child_backup_data, status_rx, program_start_time));

    backup_folder(
        &backup_data,
        PathBuf::new(),
        &backup_tx,
        &status_tx,
        &get_git_repo(&backup_data.origin),
    );

    drop(backup_tx);
    handle1.join().unwrap();
    drop(status_tx);
    handle2.join().unwrap();
}

struct StatusBar<'a> {
    handle: StdoutLock<'a>,
    start_time: SystemTime,
    width: u16,
}
impl StatusBar<'_> {
    fn new(program_start_time: SystemTime) -> Self {
        let stdout = io::stdout();
        Self {
            handle: stdout.lock(),
            start_time: program_start_time,
            width: terminal::window_size()
                .expect("Please run in terminal")
                .width,
        }
    }
    fn update_status_bars(
        &mut self,
        event: StatusUpdate,
        prog1: i32,
        tot1: i32,
        prog2: i32,
        tot2: i32,
    ) {
        if (event.should_format()) {
            println!("{}", event);
        }
        self.width = terminal::window_size()
            .expect("Please run in terminal")
            .width;
        self.printstatus(prog1, tot1);
        self.printstatus(prog2, tot2);
        execute!(self.handle, MoveUp(2)).expect("Please run in terminal!");
    }

    fn printstatus(&self, prog: i32, tot: i32) {
        let time_passed = SystemTime::now()
            .duration_since(self.start_time)
            .expect("System clock has gone backwards!");

        let eta = time_passed.mul_f32(tot as f32 / prog as f32);

        let ending = format!("] | {}/{} | {:?}/{:?}", prog, tot, time_passed, eta);
        let prog_width = self.width as usize - ending.len() - 1;
        let scaled_prog = (prog as f32 / tot as f32) as usize * prog_width;

        println!(
            "[{}{}{}",
            "-".repeat(scaled_prog),
            " ".repeat(prog_width - scaled_prog),
            ending
        );
    }
}

fn update_status(
    backup_data: &BackupData,
    status_rx: mpsc::Receiver<StatusUpdate>,
    program_start_time: SystemTime,
) {
    let mut status_bars = StatusBar::new(program_start_time);
    let mut backup_data = backup_data.clone();
    let destination_backup_log = backup_data.destination.clone().join(".backup/backup.json");
    let (mut added_files, mut modified_files, mut deleted_files, mut failed_files) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());

    let mut copied_files = 0;
    let mut expected_total_files: i32 = backup_data.files as i32;
    let mut scanned_folders = 0;
    let mut expected_total_folders: i32 = backup_data.folders as i32;

    for recv in status_rx {
        match recv.clone() {
            StatusUpdate::FileAdded(str) => {
                added_files.push(str);
                copied_files += 1;
            }
            StatusUpdate::FileFailed(str) => {
                failed_files.push(str);
                copied_files += 1
            }
            StatusUpdate::FileDeleted(str) => {
                deleted_files.push(str);
            }
            StatusUpdate::FileUnchanged() => copied_files += 1,
            StatusUpdate::FileModified(str) => modified_files.push(str),
            StatusUpdate::FolderDiscovered(delta_folders, delta_files) => {
                expected_total_files += delta_files;
                expected_total_folders += delta_folders;
            }
            StatusUpdate::FolderFailed(str) => {
                failed_files.push(str);
                scanned_folders += 1;
            }
            StatusUpdate::FolderDeleted(str) => deleted_files.push(str),
            StatusUpdate::FolderScanned() => {
                scanned_folders += 1;
            }
        }
        status_bars.update_status_bars(
            recv,
            scanned_folders,
            expected_total_folders,
            copied_files,
            expected_total_files,
        );
    }

    let backup_result = BackupResult {
        added_files,
        modified_files,
        deleted_files,
        failed_files,
        total_files: expected_total_files as usize,
        time_taken: SystemTime::now()
            .duration_since(program_start_time)
            .expect("System time has gone backwards!"),
        backup_time: program_start_time,
    };

    backup_data.last_backup = program_start_time;

    let mut destination_path_log = backup_data
        .destination
        .clone()
        .join(".backup/".to_owned() + convert_datetime_to_string(backup_data.last_backup).as_str());
    destination_path_log.set_extension("log");

    let Result::Ok(path_log) = File::create_new(&destination_path_log) else {
        panic!("Cannot create a log file in destination!");
    };
    make_read_only(&destination_path_log)
        .expect("Should be able to change permissions in destination log!");

    write_result(backup_result, path_log).expect("Should be able to write to path location!");

    let mut file = File::create(&destination_backup_log)
        .expect("Unexpected error occured in saving backup file");

    let serialise_backup_data = SerializableBackupData {
        origin: String::from(backup_data.origin.to_str().unwrap()),
        destination: String::from(backup_data.destination.to_str().unwrap()),
        last_backup: backup_data.last_backup,
        files: expected_total_files as usize,
        folders: expected_total_folders as usize,
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
