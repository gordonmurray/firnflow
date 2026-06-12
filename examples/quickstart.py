"""firn quickstart: local, no infrastructure.

Each row carries a vector (your embedding) plus optional text. Search by
vector (nearest-neighbour), by text (BM25 full-text), or both at once
(hybrid). The full-text index is built on the first text search.
"""

import firn

# No server, no infrastructure: writes to ./firn_data_demo locally.
db = firn.connect("./firn_data_demo")

# Bring your own vectors. Tiny 4-dim toy vectors here, each with text.
db.add(
    [
        {"id": 1, "vector": [1.0, 0.0, 0.0, 0.0], "text": "the quick brown fox"},
        {"id": 2, "vector": [0.0, 1.0, 0.0, 0.0], "text": "a lazy dog sleeps"},
        {"id": 3, "vector": [0.0, 0.0, 1.0, 0.0], "text": "the fox runs fast"},
    ]
)

print("full-text search for 'fox':")
for hit in db.search("fox"):
    print(f"  id={hit.id}  score={hit.score:.4f}  text={hit.text!r}")

print("\nvector search near [1, 0, 0, 0]:")
for hit in db.search(vector=[1.0, 0.0, 0.0, 0.0], limit=2):
    print(f"  id={hit.id}  score={hit.score:.4f}  text={hit.text!r}")

print("\nhybrid (text + vector):")
for hit in db.search("fox", vector=[1.0, 0.0, 0.0, 0.0], limit=3):
    print(f"  id={hit.id}  score={hit.score:.4f}  text={hit.text!r}")

db.close()
print("\nok")
