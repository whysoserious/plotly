from __future__ import annotations

import uuid
from dataclasses import dataclass

from fastapi import FastAPI, HTTPException, UploadFile, status
from pydantic import BaseModel
from starlette.status import HTTP_201_CREATED, HTTP_400_BAD_REQUEST, HTTP_413_CONTENT_TOO_LARGE

ALLOWED_EXTENSIONS = {".svg"}
ALLOWED_MIME_TYPES = {"image/svg+xml", "application/svg+xml", "text/xml", "application/xml"}
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
    suffix = file.filename.lower().rsplit(".", 1)
    if (len(suffix) != 2 or f".{suffix[1]}" not in ALLOWED_EXTENSIONS):
        raise HTTPException(
            status_code=HTTP_400_BAD_REQUEST,
            detail=f"Unsupported file extension. Allowed: {sorted(ALLOWED_EXTENSIONS)}",
        )
    if file.content_type and file.content_type not in ALLOWED_MIME_TYPES:
        raise HTTPException(
            status_code=HTTP_400_BAD_REQUEST,
            detail=f"Unsupported content type: {file.content_type}",
        )

@app.post(
    "/api/files",
    response_model=FileMetadata,
    status_code=HTTP_201_CREATED,
)
async def upload_file(file: UploadFile) -> FileMetadata:
    _validate_svg(file)

    content = await file.read()
    if len(content) > MAX_UPLOAD_BYTES:
        raise HTTPException(
            status_code=HTTP_413_CONTENT_TOO_LARGE,
            detail=f"File too large. Max: {MAX_UPLOAD_BYTES} bytes",
        )

    stored = StoredFile(
        id=str(uuid.uuid4()),
        filename=file.filename or "unknown.svg",
        content=content,
        content_type=file.content_type or "image/svg+xml",
    )
    storage.add(stored)

    return FileMetadata(
        id=stored.id,
        filename=stored.filename,
        size=stored.size,
        content_type=stored.content_type
    )
