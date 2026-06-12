"""Type stubs for the native firn extension module (`firn._native`)."""

from typing import Optional

class Hit:
    """A single search hit."""

    id: int
    score: float
    text: Optional[str]
    vector: Optional[list[float]]
    ingested_at_micros: Optional[int]
    def __repr__(self) -> str: ...

class Collection:
    """A handle to one named collection."""

    def add(self, documents: list[dict], *, tenant: Optional[str] = None) -> int: ...
    def upsert(self, documents: list[dict], *, tenant: Optional[str] = None) -> int: ...
    def delete(
        self, ids: Optional[list[int]] = None, *, tenant: Optional[str] = None
    ) -> int: ...
    def search(
        self,
        query: Optional[str] = None,
        *,
        vector: Optional[list[float]] = None,
        vectors: Optional[list[list[float]]] = None,
        hybrid: bool = False,
        limit: int = 10,
        tenant: Optional[str] = None,
        include_vectors: bool = False,
    ) -> list[Hit]: ...

class Client:
    """An embedded firn client over a default collection."""

    def collection(self, name: str) -> Collection: ...
    def add(self, documents: list[dict], *, tenant: Optional[str] = None) -> int: ...
    def upsert(self, documents: list[dict], *, tenant: Optional[str] = None) -> int: ...
    def delete(
        self, ids: Optional[list[int]] = None, *, tenant: Optional[str] = None
    ) -> int: ...
    def search(
        self,
        query: Optional[str] = None,
        *,
        vector: Optional[list[float]] = None,
        vectors: Optional[list[list[float]]] = None,
        hybrid: bool = False,
        limit: int = 10,
        tenant: Optional[str] = None,
        include_vectors: bool = False,
    ) -> list[Hit]: ...
    def close(self) -> None: ...
    def __enter__(self) -> "Client": ...
    def __exit__(self, *exc: object) -> bool: ...

def connect(
    path: Optional[str] = None,
    *,
    storage_url: Optional[str] = None,
    access_key: Optional[str] = None,
    secret_key: Optional[str] = None,
    endpoint: Optional[str] = None,
    region: Optional[str] = None,
) -> Client: ...

class FirnError(Exception): ...
class StorageError(FirnError): ...
class TenantError(FirnError): ...
class ValidationError(FirnError): ...
class UnsupportedError(FirnError): ...
