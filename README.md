# AI Vergis Backup Script

This is a script which can quickly create a copy of a folder in another location (such as a USB).

Assumptions and features:
- Only copied files modified since last backup
- Produced output must only be modified by the backup script.
- Files are periodically checked for hashes (the average rate of a hash being checked for a file can be updated in config.rs)
- Certain files are ignored, such as files ignored by git by default, .venv and node_modules.
- The above can be easily configured in config.rs
- .backup folders and files can be created in the directory


# How to use
After building for release, call execute the binary with the following command to start the process
```sh
./target/release/backup PATH_TO_DESTINATION \[PATH_TO_ORIGIN\] \[-s\]
```

If this is the first time running the script, the destination folder cannot already exist, and the origin must be specified. After this, only the destination path must be specified.
If -s is specified, the program runs silently.

A log file is produced in a .backup folder in the destination.
