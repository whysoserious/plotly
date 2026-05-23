from __future__ import annotations

import uuid
from dataclasses import dataclass, field

from fastapi import FastAPI, HTTPException, UploadFile, status
from pydantic import BaseModel

ALLOWED_EXTENSIONS = {".svg"}
ALLOWED_MIME_TYPE = {"image/svg+xml", "application/svg+xml", "text/xml", "application/xml"}
MAX_UPLOAD_BYTES = 10 * 1024 * 1024

@dataclass
class StoredFile:
    id: str
    filename: str
    content: bytes
    content_type: str

    @property
    def size(self) -> int:
        return len(self.content)

class FileMetadata(BaseModel):
    id: str
    filename: str
    size: int
    content_type: str

class FileStorage:
    def __init__(self) -> None:
        self._files: dict[str, StoredFile] = {}

    def add(self, file: StoredFile) -> None:
        self._files[file.id] = file

    def get(self, file_id: str) -> StoredFile | None:
        return self._files.get(file_id)

    def list_all(self) -> list[StoredFile]:
        return list(self._files.values())

storage = FileStorage()
app = FastAPI(title="Plotly", version="0.1.0")


def _validate_svg(file: UploadFile) -> None:
    if not file.filename:
        raise HTTPException(
            status_code=status.HTTP_400_BAD_REQUEST,
            detail="Filename is required",
        )
