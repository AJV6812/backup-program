use std::{
    fmt::Display,
    fs::File,
    io::{self, StdoutLock, Write},
    sync::mpsc,
    time::{Duration, SystemTime},
};

use crossterm::{
    cursor::MoveUp,
    execute,
    terminal::{Clear, ClearType},
};
use millisecond::{prelude::*, pretty::SecondsOptions::Separate};

use crate::{
    BackupData, BackupResult, SerializableBackupData, convert_datetime_to_string, make_read_only,
    write_result,
};

#[derive(Clone, Debug)]
pub enum StatusUpdate {
    FileAdded(String),
    FileModified(String),
    FileDeleted(String),
    FileFailed(String),
    FileUnchanged(),
    FolderDiscovered(i32, i32),
    FolderScanned(),
    FolderFailed(String),
    FolderDeleted(String, i32, i32),
    ScanFinished(),
}
impl StatusUpdate {
    fn should_format(&self) -> bool {
        match self {
            StatusUpdate::FileAdded(_) => true,
            StatusUpdate::FolderDeleted(_, _, _) => true,
            StatusUpdate::FolderFailed(_) => true,
            StatusUpdate::FileFailed(_) => true,
            StatusUpdate::FileDeleted(_) => true,
            StatusUpdate::FileModified(_) => true,
            StatusUpdate::FileUnchanged() => false,
            StatusUpdate::FolderScanned() => false,
            StatusUpdate::FolderDiscovered(_, _) => false,
            StatusUpdate::ScanFinished() => false,
        }
    }
}
impl Display for StatusUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        match self {
            StatusUpdate::FileAdded(str) => write!(f, "ADDED: {str}"),
            StatusUpdate::FolderDeleted(str, _, _) => write!(f, "DELETED: {str}"),
            StatusUpdate::FolderFailed(str) => write!(f, "FAILED: {str}"),
            StatusUpdate::FileFailed(str) => write!(f, "FAILED: {str}"),
            StatusUpdate::FileDeleted(str) => write!(f, "DELETED: {str}"),
            StatusUpdate::FileModified(str) => write!(f, "OVERWRITTEN: {str}"),
            StatusUpdate::FileUnchanged() => Err(std::fmt::Error),
            StatusUpdate::FolderScanned() => Err(std::fmt::Error),
            StatusUpdate::FolderDiscovered(_, _) => Err(std::fmt::Error),
            StatusUpdate::ScanFinished() => Err(std::fmt::Error),
        }
    }
}
struct StatusBar<'a> {
    handle: StdoutLock<'a>,
    start_time: SystemTime,
    width: u16,
    duration_options: MillisecondOption<'a>,
}
impl StatusBar<'_> {
    fn get_width() -> u16 {
        match terminal_size::terminal_size() {
            Some(dim) => dim.0.0,
            None => 30,
        }
    }
    fn new(program_start_time: SystemTime) -> Self {
        let stdout = io::stdout();
        Self {
            handle: stdout.lock(),
            start_time: program_start_time,
            width: Self::get_width(),
            duration_options: MillisecondOption {
                unit_count: Some(2),
                days_instead_of_years: false,
                dominant_only: false,
                format: millisecond::pretty::OutputFormat::Short,
                format_sub_milliseconds: false,
                separator: millisecond::pretty::Separator::Default,
                seconds: Separate,
            },
        }
    }
    fn update_status_bars(
        &mut self,
        event: StatusUpdate,
        prog1: i32,
        tot1: i32,
        prog2: i32,
        tot2: i32,
        scan_in_progress: bool,
    ) {
        if event.should_format() {
            execute!(self.handle, Clear(ClearType::FromCursorDown)).expect("Please use terminal!");
            println!("{}", event);
        }
        self.width = Self::get_width();

        let mut printed = if scan_in_progress {
            self.printstatus("SCAN", prog1, tot1) as u16
        } else {
            0
        };
        printed += self.printstatus("COPY", prog2, tot2) as u16;
        //print!("\r\r");
        execute!(self.handle, MoveUp(printed)).expect("Please run in terminal!");
    }

    fn printstatus(&self, title: &str, prog: i32, tot: i32) -> bool {
        assert!(prog <= tot);

        let time_passed = SystemTime::now()
            .duration_since(self.start_time)
            .expect("System clock has gone backwards!");

        let eta = if prog != 0 {
            time_passed.mul_f32(tot as f32 / prog as f32)
        } else {
            Duration::new(0, 0)
        };

        let time_left = if eta >= time_passed {
            eta - time_passed
        } else {
            Duration::new(0, 0)
        };

        let beginning = format!("{}: [", title);
        let ending = format!(
            "] | {}/{} | {}<{}",
            prog,
            tot,
            time_passed.pretty_with(self.duration_options),
            time_left.pretty_with(self.duration_options)
        );

        if self.width as usize >= ending.len() + 1 + beginning.len() {
            let prog_width = self.width as usize - ending.len() - 1 - beginning.len();
            let scaled_prog = if tot != 0 {
                ((prog * prog_width as i32) as f32 / tot as f32) as usize
            } else {
                0
            };

            println!(
                "{}{}{}{}",
                beginning,
                "-".repeat(scaled_prog),
                " ".repeat(prog_width - scaled_prog),
                ending
            );
            return true;
        }
        false
    }
}

//TODO: Check that stdout is reserved for logging

pub fn update_status(
    backup_data: &BackupData,
    status_rx: mpsc::Receiver<StatusUpdate>,
    program_start_time: SystemTime,
    logging: bool,
) -> Result<BackupResult, anyhow::Error> {
    let mut status_bars = StatusBar::new(program_start_time);
    let mut backup_data = backup_data.clone();
    let destination_backup_log = backup_data.destination.clone().join(".backup/backup.json");
    let (mut added_files, mut modified_files, mut deleted_files, mut failed_files) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());

    let mut copied_files = 0;
    let mut expected_total_files: i32 = backup_data.files as i32;
    let mut scanned_folders = 0;
    let mut expected_total_folders: i32 = backup_data.folders as i32;

    let mut scan_in_progress = true;
    for recv in status_rx {
        //TODO: Throttle this function
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
            StatusUpdate::FileModified(str) => {
                modified_files.push(str);
                copied_files += 1;
            }
            StatusUpdate::FolderDiscovered(delta_folders, delta_files) => {
                expected_total_files += delta_files;
                expected_total_folders += delta_folders;
            }
            StatusUpdate::FolderFailed(str) => {
                failed_files.push(str);
                scanned_folders += 1;
            }
            StatusUpdate::FolderDeleted(str, folders_inside, files_inside) => {
                deleted_files.push(str);
                expected_total_files -= files_inside;
                expected_total_folders -= folders_inside;
            }
            StatusUpdate::FolderScanned() => {
                scanned_folders += 1;
            }
            StatusUpdate::ScanFinished() => {
                println!("");
                assert!(scan_in_progress);
                scan_in_progress = false;
            }
        }

        if copied_files > expected_total_files {
            expected_total_files = copied_files;
        }
        if scanned_folders > expected_total_folders {
            expected_total_folders = scanned_folders;
        }

        if logging {
            status_bars.update_status_bars(
                recv,
                scanned_folders,
                expected_total_folders,
                copied_files,
                expected_total_files,
                scan_in_progress,
            );
        }
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

    let mut destination_path_log = backup_data.destination.clone().join(
        ".backup/backup-log-".to_owned()
            + convert_datetime_to_string(backup_data.last_backup)
                .replace(":", "-")
                .as_str(),
    );
    destination_path_log.set_extension("log");

    let path_log = File::create_new(&destination_path_log).unwrap();
    //let Result::Ok(path_log) = File::create_new(&destination_path_log) else {
    //    panic!("Cannot create a log file in destination!");
    //};
    make_read_only(&destination_path_log)
        .expect("Should be able to change permissions in destination log!");

    write_result(&backup_result, path_log).expect("Should be able to write to path location!");

    let mut file = File::create(&destination_backup_log)
        .expect("Unexpected error occured in saving backup file");

    let serialise_backup_data = SerializableBackupData {
        origin: String::from(backup_data.origin.to_str().unwrap()),
        destination: String::from(backup_data.destination.to_str().unwrap()),
        last_backup: backup_data.last_backup,
        files: copied_files as usize,
        folders: scanned_folders as usize,
        dump_secret: backup_data.dump_secret,
    };

    file.write_all(
        serde_json::to_string(&serialise_backup_data)
            .unwrap()
            .as_bytes(),
    )
    .expect("Could not save backup data!");

    println!();
    println!();

    Ok(backup_result)
}
