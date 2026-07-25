from __future__ import annotations

import json
import logging
from uuid import uuid4

import keyring
from fastapi import APIRouter, HTTPException, Request
from pydantic import BaseModel

logger = logging.getLogger(__name__)
router = APIRouter(prefix="/api/providers")


class CreateProviderRequest(BaseModel):
    name: str
    provider_type: str  # "openai_compat" | "anthropic"
    base_url: str
    api_key: str | None = None
    fallback_priority: int = 1
    config: dict | None = None


class UpdateProviderRequest(BaseModel):
    name: str | None = None
    base_url: str | None = None
    api_key: str | None = None
    fallback_priority: int | None = None
    config: dict | None = None


@router.get("")
async def list_providers(request: Request):
    db = request.app.state.db
    rows = await db.fetch_all("SELECT * FROM providers ORDER BY fallback_priority")
    return {"providers": [dict(r) for r in rows]}


@router.post("")
async def add_provider(body: CreateProviderRequest, request: Request):
    db = request.app.state.db
    provider_id = uuid4().hex[:8]
    await db.execute(
        "INSERT INTO providers (id, name, provider_type, base_url, fallback_priority, config) "
        "VALUES (:id, :name, :type, :url, :priority, :config)",
        {
            "id": provider_id,
            "name": body.name,
            "type": body.provider_type,
            "url": body.base_url,
            "priority": body.fallback_priority,
            "config": json.dumps(body.config or {}),
        },
    )
    if body.api_key:
        keyring.set_password("bigtiny", f"{provider_id}_api_key", body.api_key)

    router = request.app.state.router
    row = {
        "id": provider_id,
        "name": body.name,
        "provider_type": body.provider_type,
        "base_url": body.base_url,
        "fallback_priority": body.fallback_priority,
        "config": body.config or {},
        "status": "disconnected",
    }
    await router.register_provider(row, body.api_key)

    return {"id": provider_id, "status": "created"}


@router.patch("/{provider_id}")
async def update_provider(provider_id: str, body: UpdateProviderRequest, request: Request):
    db = request.app.state.db
    existing = await db.fetch_one(
        "SELECT * FROM providers WHERE id = :id", {"id": provider_id}
    )
    if not existing:
        raise HTTPException(404, "Provider not found")

    updates: dict[str, object] = {}
    if body.name is not None:
        updates["name"] = body.name
    if body.base_url is not None:
        updates["base_url"] = body.base_url
    if body.fallback_priority is not None:
        updates["fallback_priority"] = body.fallback_priority
    if body.config is not None:
        updates["config"] = json.dumps(body.config)

    if updates:
        set_clause = ", ".join(f"{k} = :{k}" for k in updates)
        updates["id"] = provider_id
        await db.execute(
            f"UPDATE providers SET {set_clause}, updated_at = CURRENT_TIMESTAMP WHERE id = :id",
            updates,
        )

    if body.api_key:
        keyring.set_password("bigtiny", f"{provider_id}_api_key", body.api_key)

    router = request.app.state.router
    row = await db.fetch_one(
        "SELECT * FROM providers WHERE id = :id", {"id": provider_id}
    )
    if row:
        await router.register_provider(dict(row), body.api_key)

    return {"status": "updated"}


@router.delete("/{provider_id}")
async def delete_provider(provider_id: str, request: Request):
    db = request.app.state.db
    await db.execute("DELETE FROM providers WHERE id = :id", {"id": provider_id})
    try:
        keyring.delete_password("bigtiny", f"{provider_id}_api_key")
    except Exception:
        pass

    router = request.app.state.router
    router.unregister_provider(provider_id)

    return {"status": "deleted"}


@router.post("/{provider_id}/test")
async def test_provider(provider_id: str, request: Request):
    router = request.app.state.router
    provider = router._providers.get(provider_id)
    if not provider:
        raise HTTPException(404, "Provider not found")
    try:
        models = await provider.discover_models()
        return {
            "status": "connected",
            "models": [m.model_dump() for m in models],
        }
    except Exception as e:
        return {"status": "error", "error": str(e)}


@router.get("/{provider_id}/models")
async def list_models(provider_id: str, request: Request):
    router = request.app.state.router
    provider = router._providers.get(provider_id)
    if not provider:
        raise HTTPException(404, "Provider not found")
    models = await provider.discover_models()
    return {"models": [m.model_dump() for m in models]}
