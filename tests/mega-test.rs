use std::{
    fs::{self, File},
    io::{Read, Write},
    path::PathBuf,
    thread::{self, sleep},
    time::Duration,
};

use git2::Repository;

struct TestData {
    test_folder: PathBuf,
    dest: String,
    origin: String,
}

const RELATIVE_BACKUP_DUMP_LOCATION: &str = ".backup/backup.json";
const RELATIVE_UUIDS_LOCATION: &str = ".backup_uuids.txt";

impl TestData {
    fn new(test_name: &str) -> Self {
        let test_data = Self {
            test_folder: PathBuf::from(format!("test-fs/{test_name}")),
            dest: format!("test-fs/{test_name}/destination").to_string(),
            origin: format!("test-fs/{test_name}/origin").to_string(),
        };

        assert!(!fs::exists(&test_data.test_folder).unwrap());
        fs::create_dir_all(&test_data.test_folder).unwrap();

        let origin_folder = format!("{}/folder", test_data.origin);
        let origin_folder_file = format!("{}/data.txt", origin_folder);
        let origin_file = format!("{}/data.txt", test_data.origin);

        fs::create_dir(&test_data.origin).unwrap();
        fs::create_dir(origin_folder).unwrap();
        let mut f = File::create(origin_file).unwrap();
        writeln!(f, "Stuff").unwrap();
        let mut f = File::create(origin_folder_file).unwrap();
        writeln!(f, "More stuff").unwrap();

        test_data
    }
}

impl Drop for TestData {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.test_folder).unwrap();
    }
}

fn files_equal(path1: &str, path2: &str) -> Result<bool, anyhow::Error> {
    let mut buf1 = String::new();
    let mut buf2 = String::new();
    let mut file1 = File::open(path1)?;
    let mut file2 = File::open(path2)?;
    file1.read_to_string(&mut buf1)?;
    file2.read_to_string(&mut buf2)?;

    Ok(buf1 == buf2)
}

#[test]
#[should_panic(expected = "Could not locate a destination backup dump")]
fn destination_deleted() {
    let test_data = TestData::new("test1");

    backup::attempt_full_backup(&test_data.dest, &test_data.origin, false).unwrap();

    fs::remove_dir_all(&test_data.dest).unwrap();

    backup::attempt_update_backup(&test_data.dest, false).unwrap();
}

//TODO: Panic message for below test is incorrect
#[test]
#[should_panic(expected = "Should be able to read origin secrets")]
fn origin_deleted_destination_exists() {
    let test_data = TestData::new("test2");

    backup::attempt_full_backup(&test_data.dest, &test_data.origin, false).unwrap();

    fs::remove_dir_all(&test_data.origin).unwrap();

    backup::attempt_update_backup(&test_data.dest, false).unwrap();
}

#[test]
#[should_panic(expected = "Backup Dump Corrupted")]
fn backup_dump_modified() {
    let test_data = TestData::new("test3");

    backup::attempt_full_backup(&test_data.dest, &test_data.origin, false).unwrap();

    let backup_dump = format!("{}/{RELATIVE_BACKUP_DUMP_LOCATION}", test_data.dest);
    let mut f = File::options().append(true).open(backup_dump).unwrap();
    writeln!(&mut f, "new line").unwrap();

    backup::attempt_update_backup(&test_data.dest, false).unwrap();
}

#[test]
fn assumption_checking() {
    let test_data = TestData::new("test4");

    backup::attempt_full_backup(&test_data.dest, &test_data.origin, false).unwrap();

    let backup_dump = format!("{}/{RELATIVE_BACKUP_DUMP_LOCATION}", test_data.dest);

    let uuids = format!("{}/{RELATIVE_UUIDS_LOCATION}", test_data.origin);
    assert!(fs::exists(backup_dump).unwrap());
    assert!(fs::exists(uuids).unwrap());
}

#[test]
#[should_panic(expected = "Origin secrets should contain uuids")]
fn uuids_replaced_destination_exists() {
    let test_data = TestData::new("test5");

    backup::attempt_full_backup(&test_data.dest, &test_data.origin, false).unwrap();

    let uuids = format!("{}/{RELATIVE_UUIDS_LOCATION}", test_data.origin);
    fs::remove_file(&uuids).unwrap();
    let mut f = File::create(uuids).unwrap();
    writeln!(&mut f, "new line").unwrap();

    backup::attempt_update_backup(&test_data.dest, false).unwrap();
}

#[test]
#[should_panic(expected = "Cannot find folder to backup!")]
fn origin_deleted_destination_does_not_exist() {
    let test_data = TestData::new("test6");

    fs::remove_dir_all(&test_data.origin).unwrap();
    backup::attempt_full_backup(&test_data.dest, &test_data.origin, false).unwrap();

    backup::attempt_update_backup(&test_data.dest, false).unwrap();
}

#[test]
#[should_panic(expected = "Should be able to read origin secrets")]
fn uuids_deleted_destination_exists() {
    let test_data = TestData::new("test7");

    backup::attempt_full_backup(&test_data.dest, &test_data.origin, false).unwrap();

    let uuids = format!("{}/{RELATIVE_UUIDS_LOCATION}", test_data.origin);
    fs::remove_file(uuids).unwrap();

    backup::attempt_update_backup(&test_data.dest, false).unwrap();
}

#[test]
#[should_panic(expected = "Could not locate a destination backup dump")]
fn destination_folder_becomes_file() {
    let test_data = TestData::new("test8");

    backup::attempt_full_backup(&test_data.dest, &test_data.origin, false).unwrap();

    fs::remove_dir_all(&test_data.dest).unwrap();
    let mut file = File::create(&test_data.dest).unwrap();
    writeln!(file, "cool data").unwrap();

    backup::attempt_update_backup(&test_data.dest, false).unwrap();
}

#[test]
#[should_panic(expected = "Should be able to read origin secrets")]
fn origin_folder_becomes_file() {
    let test_data = TestData::new("test9");

    backup::attempt_full_backup(&test_data.dest, &test_data.origin, false).unwrap();

    fs::remove_dir_all(&test_data.origin).unwrap();
    let mut file = File::create(&test_data.origin).unwrap();
    writeln!(file, "cool data").unwrap();

    backup::attempt_update_backup(&test_data.dest, false).unwrap();
}

#[test]
fn git_ignored_files() {
    let test_data = TestData::new("test10");

    let repo_path = format!("{}/git_repo", test_data.origin);
    let dest_repo_path = format!("{}/git_repo", test_data.dest);

    let repo = Repository::init(&repo_path).unwrap();

    let temp_path = format!("{}/temp.txt", repo_path);
    let long_temp_path = format!("{}/temp/log.txt", repo_path);
    let perst_path = format!("{}/perst.txt", repo_path);

    let dest_temp_path = format!("{}/temp.txt", dest_repo_path);
    let dest_long_temp_path = format!("{}/temp/log.txt", dest_repo_path);
    let dest_perst_path = format!("{}/perst.txt", dest_repo_path);

    let temp_file = File::create(&temp_path).unwrap();
    let perst_file = File::create(&perst_path).unwrap();
    fs::create_dir(format!("{}/temp", repo_path)).unwrap();
    let long_temp_file = File::create(&long_temp_path).unwrap();
    writeln!(&temp_file, "Hi there").unwrap();
    writeln!(&long_temp_file, "Hi there").unwrap();
    writeln!(&perst_file, "Hello there!").unwrap();

    assert!(!fs::exists(&dest_temp_path).unwrap());
    assert!(!fs::exists(&dest_long_temp_path).unwrap());
    assert!(!fs::exists(&dest_perst_path).unwrap());

    backup::attempt_full_backup(&test_data.dest, &test_data.origin, false).unwrap();
    assert!(fs::exists(&dest_temp_path).unwrap());
    assert!(fs::exists(&dest_long_temp_path).unwrap());
    assert!(fs::exists(&dest_perst_path).unwrap());

    let mut file = File::create(format!("{}/.gitignore", repo_path)).unwrap();
    writeln!(file, "temp*").unwrap();
    repo.add_ignore_rule("temp*\n").unwrap();
    drop(repo);

    backup::attempt_update_backup(&test_data.dest, false).unwrap();

    assert!(!fs::exists(dest_temp_path).unwrap());
    assert!(!fs::exists(dest_long_temp_path).unwrap());
    assert!(!fs::exists(format!("{}/temp", dest_repo_path)).unwrap());
    assert!(fs::exists(dest_perst_path).unwrap());
}

#[test]
fn multiple_backups() {
    let test_data = TestData::new("test11");

    backup::attempt_full_backup(&test_data.dest, &test_data.origin, false).unwrap();

    let dest2 = format!("{}/dest2", test_data.test_folder.to_str().unwrap());

    backup::attempt_full_backup(&dest2, &test_data.origin, false).unwrap();
    backup::attempt_update_backup(&test_data.dest, false).unwrap();
}

#[test]
fn file_modified_during_test() -> Result<(), anyhow::Error> {
    let test_data = TestData::new("test12");

    let child_dest = test_data.dest.clone();
    let child_origin = test_data.origin.clone();
    let origin_mod_path = format!("{}/data.txt", test_data.origin);
    let origin_new_path = format!("{}/data2.txt", test_data.origin);
    let dest_mod_path = format!("{}/data.txt", test_data.dest);
    let dest_new_path = format!("{}/data2.txt", test_data.dest);

    let handle1 =
        thread::spawn(move || backup::attempt_full_backup(&child_dest, &child_origin, false));
    sleep(Duration::new(0, 2000000000));

    let mut file = File::options().append(true).open(&origin_mod_path).unwrap();
    writeln!(file, "Extra stuff").unwrap();
    drop(file);
    let mut file2 = File::create(&origin_new_path).unwrap();
    writeln!(file2, "Content").unwrap();
    drop(file2);

    handle1.join().unwrap().unwrap();

    assert!(!files_equal(&origin_mod_path, &dest_mod_path)?);
    assert!(!fs::exists(&dest_new_path)?);

    backup::attempt_update_backup(&test_data.dest, false).unwrap();

    assert!(files_equal(&origin_mod_path, &dest_mod_path)?);
    assert!(files_equal(&origin_new_path, &dest_new_path)?);
    Ok(())
}
