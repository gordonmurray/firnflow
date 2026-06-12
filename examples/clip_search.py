"""firn + OpenCLIP photo search on Tigris.

Embeds a few sample photos with OpenCLIP, stores the image embeddings in
firn (whose tables live on Tigris) and the photo bytes as Tigris objects,
then searches by text and by image.

CLIP puts images and text in the same vector space, so a text query like
"a cat" finds the cat photo even though only image embeddings are stored.

Credentials come from the environment (TIGRIS_ACCESS_KEY /
TIGRIS_SECRET_KEY / TIGRIS_BUCKET).
"""

import io
import os

import boto3
import firn
import open_clip
import torch
from PIL import Image
from skimage import data

ENDPOINT = "https://t3.storage.dev"
BUCKET = os.environ.get("TIGRIS_BUCKET", "firn-tigris-bucket")
AK = os.environ["TIGRIS_ACCESS_KEY"]
SK = os.environ["TIGRIS_SECRET_KEY"]
PREFIX = "clip-demo/photos/"

# A few labelled sample photos, bundled with scikit-image (no download).
SAMPLES = {
    "cat.jpg": data.chelsea(),
    "coffee.jpg": data.coffee(),
    "astronaut.jpg": data.astronaut(),
    "rocket.jpg": data.rocket(),
}

# --- CLIP model (downloads the weights once, ~350 MB, then cached) ---
# The "-quickgelu" variant matches the openai weights' activation, which
# avoids a QuickGELU-mismatch warning.
print("loading OpenCLIP ViT-B-32 ...")
model, _, preprocess = open_clip.create_model_and_transforms(
    "ViT-B-32-quickgelu", pretrained="openai"
)
tokenizer = open_clip.get_tokenizer("ViT-B-32-quickgelu")
model.eval()


def embed_image(img: Image.Image) -> list:
    tensor = preprocess(img.convert("RGB")).unsqueeze(0)
    with torch.no_grad():
        feat = model.encode_image(tensor)
        feat /= feat.norm(dim=-1, keepdim=True)  # unit-normalise
    return feat[0].tolist()


def embed_text(text: str) -> list:
    with torch.no_grad():
        feat = model.encode_text(tokenizer([text]))
        feat /= feat.norm(dim=-1, keepdim=True)
    return feat[0].tolist()


# Tigris S3 client for the raw photo bytes.
s3 = boto3.client(
    "s3",
    endpoint_url=ENDPOINT,
    region_name="auto",
    aws_access_key_id=AK,
    aws_secret_access_key=SK,
)

# firn, with its search index living on Tigris too.
db = firn.connect(
    storage_url=f"s3://{BUCKET}/clip-demo",
    endpoint=ENDPOINT,
    region="auto",
    access_key=AK,
    secret_key=SK,
)

# --- ingest: photo bytes -> Tigris objects, embeddings -> firn ---
images = {}
docs = []
for i, (name, arr) in enumerate(SAMPLES.items(), start=1):
    img = Image.fromarray(arr).convert("RGB")
    images[name] = img
    buf = io.BytesIO()
    img.save(buf, format="JPEG")
    buf.seek(0)
    key = PREFIX + name
    s3.upload_fileobj(buf, BUCKET, key)
    docs.append({"id": i, "vector": embed_image(img), "text": key})
db.add(docs)
print(f"ingested {len(docs)} photos -> Tigris ({BUCKET}/{PREFIX}) + firn\n")

# --- search by text (lower distance = closer match) ---
print("text -> image search:")
for q in ["a cat", "a cup of coffee", "an astronaut in space", "a rocket"]:
    top = db.search(vector=embed_text(q), limit=1)[0]
    print(f"  {q!r:28} -> {top.text}  (distance {top.score:.3f})")

# --- search by image ---
q_name = "cat.jpg"
print(f"\nimage -> image search (query: {q_name}):")
for h in db.search(vector=embed_image(images[q_name]), limit=3):
    print(f"  {h.text}  (distance {h.score:.3f})")

db.close()
print("\nok")
