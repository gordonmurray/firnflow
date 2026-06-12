# firn

Vector + full-text search on object storage, embeddable in-process. No
server, no infrastructure to stand up.

```python
import firn

db = firn.connect()                 # local ./firn_data
# ...or object storage:
# db = firn.connect(storage_url="s3://bucket",
#                   access_key=..., secret_key=...)

db.add([
    {"id": 1, "vector": [0.1, 0.2, 0.3], "text": "the quick brown fox"},
    {"id": 2, "vector": [0.4, 0.5, 0.6], "text": "a lazy dog sleeps"},
])

hits = db.search("fox")                            # full-text (BM25)
hits = db.search(vector=[0.1, 0.2, 0.3])           # nearest-neighbour
hits = db.search("fox", vector=[0.1, 0.2, 0.3])    # hybrid (fused)
for hit in hits:
    print(hit.id, hit.score, hit.text)
```

Each row carries a vector (your embedding) and optional text. Search by
vector, by text, or both at once (reciprocal-rank fused). Storage is a
local directory or any S3-compatible object store (AWS, Tigris, R2,
MinIO, …) or GCS.

Apache-2.0. Part of [Firn](https://firnflow.io).
