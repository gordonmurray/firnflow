"""End-to-end tests for the firn package against local embedded mode.

No server, no cloud — each test gets its own temp data dir and an
isolated foyer cache (via XDG_CACHE_HOME).
"""

import firn
import pytest


@pytest.fixture
def db(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_CACHE_HOME", str(tmp_path / "cache"))
    client = firn.connect(str(tmp_path / "data"))
    yield client
    client.close()


def test_add_and_full_text(db):
    db.add(
        [
            {"id": 1, "vector": [1.0, 0.0, 0.0, 0.0], "text": "the quick brown fox"},
            {"id": 2, "vector": [0.0, 1.0, 0.0, 0.0], "text": "a lazy dog"},
        ]
    )
    hits = db.search("fox")
    assert hits and hits[0].id == 1


def test_upsert_latest_wins(db):
    db.add([{"id": 1, "vector": [1.0, 0.0, 0.0, 0.0], "text": "old"}])
    db.upsert([{"id": 1, "vector": [1.0, 0.0, 0.0, 0.0], "text": "new"}])
    hits = db.search(vector=[1.0, 0.0, 0.0, 0.0], limit=1)
    assert hits[0].text == "new"


def test_hybrid_guards(db):
    db.add([{"id": 1, "vector": [1.0, 0.0, 0.0, 0.0], "text": "fox"}])
    with pytest.raises(firn.ValidationError):
        db.search("fox", hybrid=True)  # no vector
    with pytest.raises(firn.ValidationError):
        db.search("fox", vector=[], hybrid=True)  # empty vector is not a vector


def test_include_vectors(db):
    db.add([{"id": 1, "vector": [1.0, 0.0, 0.0, 0.0], "text": "x"}])
    assert db.search(vector=[1.0, 0.0, 0.0, 0.0], limit=1)[0].vector is None
    got = db.search(vector=[1.0, 0.0, 0.0, 0.0], limit=1, include_vectors=True)[0]
    assert got.vector == [1.0, 0.0, 0.0, 0.0]


def test_search_filter(db):
    db.add(
        [
            {"id": 1, "vector": [1.0, 0.0, 0.0, 0.0], "text": "x"},
            {"id": 2, "vector": [0.0, 1.0, 0.0, 0.0], "text": "x"},
            {"id": 3, "vector": [0.0, 0.0, 1.0, 0.0], "text": "x"},
        ]
    )
    hits = db.search(vector=[1.0, 0.0, 0.0, 0.0], limit=3, filter="id > 1")
    assert [h.id for h in hits] == [2, 3]


def test_facet(db):
    db.add(
        [
            {
                "id": 1,
                "vector": [1.0, 0.0, 0.0, 0.0],
                "attributes": {"section": "warnings", "route": "oral"},
            },
            {
                "id": 2,
                "vector": [0.0, 1.0, 0.0, 0.0],
                "attributes": {"section": "dosage", "route": "oral"},
            },
            {
                "id": 3,
                "vector": [0.0, 0.0, 1.0, 0.0],
                "attributes": {"section": "warnings"},
            },
        ]
    )
    facets = db.facet(["section", "route"], filter="id >= 1", top=10)
    assert facets["section"][0] == {"value": "warnings", "count": 2}
    assert facets["route"][0] == {"value": "oral", "count": 2}


def test_tenant_isolation(db):
    db.add([{"id": 10, "vector": [1.0, 0.0, 0.0, 0.0], "text": "a"}], tenant="acme")
    db.add([{"id": 20, "vector": [1.0, 0.0, 0.0, 0.0], "text": "b"}], tenant="globex")
    a = {h.id for h in db.search(vector=[1.0, 0.0, 0.0, 0.0], limit=10, tenant="acme")}
    b = {h.id for h in db.search(vector=[1.0, 0.0, 0.0, 0.0], limit=10, tenant="globex")}
    assert a == {10}
    assert b == {20}


def test_bad_tenant_rejected(db):
    with pytest.raises(firn.TenantError):
        db.search(vector=[1.0, 0.0, 0.0, 0.0], tenant="bad_tenant")  # "_" is illegal


def test_collection_isolation(db):
    col = db.collection("products")
    col.add([{"id": 5, "vector": [0.0, 0.0, 1.0, 0.0], "text": "widget"}])
    assert {h.id for h in col.search("widget")} == {5}
    assert not any(h.id == 5 for h in db.search("widget"))


def test_delete(db):
    db.add([{"id": 1, "vector": [1.0, 0.0, 0.0, 0.0], "text": "x"}])
    db.delete()
    assert db.search(vector=[1.0, 0.0, 0.0, 0.0], limit=10) == []
    with pytest.raises(firn.UnsupportedError):
        db.delete(ids=[1])


def test_multivector(db):
    db.add(
        [
            {"id": 1, "vectors": [[1.0, 0.0], [1.0, 0.0]], "text": "a"},
            {"id": 2, "vectors": [[0.0, 1.0], [0.0, 1.0]], "text": "b"},
        ]
    )
    hits = db.search(vectors=[[1.0, 0.0]], limit=2)
    assert hits[0].id == 1
    assert hits[0].vector is None
    with pytest.raises(firn.ValidationError):
        db.add([{"id": 9, "vector": [1.0], "vectors": [[1.0]]}])  # both
    with pytest.raises(firn.ValidationError):
        db.add([{"id": 8, "text": "no vector"}])  # neither


def test_close_is_idempotent_and_blocks_use(tmp_path, monkeypatch):
    monkeypatch.setenv("XDG_CACHE_HOME", str(tmp_path / "cache"))
    client = firn.connect(str(tmp_path / "data"))
    client.add([{"id": 1, "vector": [1.0, 0.0], "text": "x"}])
    client.close()
    client.close()  # idempotent
    with pytest.raises(firn.FirnError):
        client.search(vector=[1.0, 0.0])
    # Closed is checked before other validation: this is the closed
    # error, not UnsupportedError from the ids guard.
    with pytest.raises(firn.FirnError) as exc:
        client.delete(ids=[1])
    assert not isinstance(exc.value, firn.UnsupportedError)


def test_concurrent_search_and_close(tmp_path, monkeypatch):
    """Closing while searches run from other threads must not crash or
    use the cache after close — the lifecycle gate makes close wait."""
    import threading

    monkeypatch.setenv("XDG_CACHE_HOME", str(tmp_path / "cache"))
    client = firn.connect(str(tmp_path / "data"))
    client.add([{"id": i, "vector": [float(i), 0.0], "text": "x"} for i in range(5)])

    errors = []

    def worker():
        for _ in range(20):
            try:
                client.search(vector=[1.0, 0.0], limit=3)
            except firn.FirnError:
                return  # closed mid-run is fine
            except Exception as e:  # nothing else should escape
                errors.append(e)
                return

    threads = [threading.Thread(target=worker) for _ in range(3)]
    for t in threads:
        t.start()
    client.close()  # races with in-flight searches
    for t in threads:
        t.join(timeout=10)
    assert not errors, errors
    with pytest.raises(firn.FirnError):
        client.search(vector=[1.0, 0.0])
