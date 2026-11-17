use backup::{attempt_full_backup, attempt_update_backup};
use std::{env, io};

fn main() {
    let args: Vec<String> = env::args().collect();

    let silent = args.contains(&String::from("-s"));
    let args: Vec<_> = args.iter().filter(|arg| **arg != "-s").collect();

    if let Err(error) = attempt_update_backup(
        args.get(1).expect("Must pass in destination as argument"),
        !silent,
    ) {
        println!("Could not load previous backup due to {error}!");
        println!("Backup from scratch (Y/N)? ");
        let mut res = String::new();
        io::stdin()
            .read_line(&mut res)
            .expect("Could not read from stdin!");
        if !res.to_lowercase().contains("y") {
            return;
        }
        attempt_full_backup(
            args.get(1).expect("Must pass in destination as argument"),
            args.get(2).expect("Expecting origin for full backup!"),
            !silent,
        )
        .expect("Full backup failed!");
    }
}
