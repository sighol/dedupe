#!/usr/bin/python3

import argparse
from collections import defaultdict
import hashlib
import logging
import sys
from pathlib import Path

import sqlalchemy as db, select
from sqlalchemy import Column, Integer, String, create_engine, Boolean
from sqlalchemy.orm import Session, declarative_base
from sqlalchemy.sql import text

try:
    import coloredlogs

    coloredlogs.install(
        milliseconds=False,
        stream=sys.stdout,
        isatty=True,
        fmt="%(asctime)s.%(msecs)03d %(name)s %(levelname)s %(message)s",
    )
except ImportError:
    logging.basicConfig(
        level=logging.INFO,
        datefmt="%Y-%m-%d %H:%M:%S",
        format="%(asctime)s.%(msecs)03d %(name)s %(levelname)s %(message)s",
        handlers=[logging.StreamHandler(sys.stdout)],
        force=True,
    )

log = logging.getLogger(__name__)

Base = declarative_base()

IMAGE_FILES = [".png", ".jpg", ".jpeg", ".tif", ".pdf"]


class File(Base):
    __tablename__ = "files"
    id = Column(Integer, autoincrement=True, primary_key=True)
    file_path = Column(String(1024))
    file_hash = Column(String(512), nullable=True)
    file_len = Column(Integer)
    is_deleted = Column(Boolean)

    def __repr__(self):
        return f"{self.file_path}: {self.file_len}, {self.file_hash}"


def add_folder(session: Session, path: Path):
    if not path.exists():
        log.info(f"Path {path} does not exist")
        return
    for file in path.iterdir():
        if file.is_dir():
            add_folder(session, file)
        elif file.suffix.lower() in IMAGE_FILES:
            add_file(session, file)


def add_file(session: Session, path: Path):
    assert path.is_file()

    prev = session.query(File).filter_by(file_path=str(path.absolute())).first()
    if prev:
        log.info(f"File {path.absolute()} is already added")
        return
    else:
        log.info(f"File {path.absolute()} is new")

        file = File(
            file_path=str(path.absolute()),
            file_len=path.stat().st_size,
            is_deleted=False,
        )
        session.add(file)
    session.commit()


def rescan_same_size(session: Session):
    files = session.query(File).filter_by(is_deleted=False).all()

    grouped_by_size: dict[int, list[File]] = {}
    for file in files:
        grouped_by_size[file.file_len] = grouped_by_size.get(file.file_len, []) + [file]
    for key, values in grouped_by_size.items():
        if len(values) > 1:
            print()
            for v in values:
                if not v.file_hash:
                    checksum = calculate_md5(Path(v.file_path))
                    v.file_hash = checksum
    session.commit()

def calculate_md5(file_path):
    # Open the file in binary mode and read its contents
    with open(file_path, "rb") as file:
        # Create an MD5 hash object
        md5_hash = hashlib.md5()

        # Read the file in chunks to avoid loading the entire file into memory
        for chunk in iter(lambda: file.read(4096), b""):
            # Update the MD5 hash with the read chunk
            md5_hash.update(chunk)

    # Return the hexadecimal representation of the computed MD5 hash
    return md5_hash.hexdigest()

def find_duplicates(session: Session):
    files: list[File] = session.query(File).filter(File.file_hash != None, File.is_deleted == False).all()

    groups = defaultdict(lambda: [])
    for file in files:
        groups[file.file_hash].append(file)

    for _, group in groups.items():
        for g in group:
            print(g)
        print()

def main():
    p = argparse.ArgumentParser()
    p.add_argument("PATH", nargs="+")
    p.add_argument("--db_path", default="dedupe.db")
    args = p.parse_args()

    engine = create_engine(f"sqlite:///{args.db_path}", echo=False, future=True)
    session = Session(engine)
    Base.metadata.create_all(engine)

    # for path in args.PATH:
    #     add_folder(session, Path(path))

    rescan_same_size(session)
    find_duplicates(session)


if __name__ == "__main__":
    main()
