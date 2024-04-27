#!/usr/bin/python3

import argparse
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


def main():
    p = argparse.ArgumentParser()
    p.add_argument("PATH", nargs="+")
    p.add_argument("--db_path", default="dedupe.db")
    args = p.parse_args()

    engine = create_engine(f"sqlite:///{args.db_path}", echo=False, future=True)
    session = Session(engine)
    Base.metadata.create_all(engine)

    for path in args.PATH:
        add_folder(session, Path(path))


if __name__ == "__main__":
    main()
