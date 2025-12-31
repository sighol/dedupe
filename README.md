# Dedupe

CLI utility for finding duplicated files.

Bad code quality weekend project to scratch an itch. I think it works, but not tested very well.

**NB**: Use at your own risk.

# Usage

## Find duplicated files in folder

```shell
dedupe FOLDER1 FOLDER2
```

## Check if folder only contains duplicates

```shell
dedupe FOLDER1 FOLDER2 --report-dir FOLDER1/test/sub/folder
```

It will then report how many files are unique to `FOLDER1/test/sub/folder` and how many are
duplicates from other places in FOLDER1 and FOLDER2.

## Automatic cleanup by adding a score to each file

Add scoring of files based on the file name. For a duplication group, it only keeps the file with the
highest scoring file name, and delete the rest.

To add a score:

```shell
dedupe FOLDER1 FOLDER2 --score=50=MY_REGEX --score=100=MY_OTHER_REGEX
```

Each file will get the score of the first regex that matches. Files from a duplication group will
only be deleted if all files in the group has a score and not all files have the same score.

To perform deletion:

```shell
dedupe FOLDER1 FOLDER2 --score=50=MY_REGEX --score=100=MY_OTHER_REGEX --delete
```

## How it works

1. Iterate through the directories recursively and retrieves file paths and file sizes.
2. Remove files that are empty. All empty files are duplicate anyway.
3. Find and retain the files that are duplicated by size.
4. Hash the first 4096 bytes of the files.
5. Retain the files that are duplicated by the hash of the first 4096 bytes.
6. Hash the full content of the files.
7. Group by the full hash and report duplicates.

# TODO

## Clean up CLI interface

It feels a bit messy. Maybe group something into sub-commands.

Maybe something like:

```shell
dedupe duplicates FOLDER1 FOLDER2
dedupe delete-duplicates (--force) FOLDER1 FOLDER2
dedupe check-folder FOLDER1 FOLDER2
```

Doesn't feel amazing that one either.
