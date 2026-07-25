from __future__ import annotations

import json
import logging
from uuid import uuid4

from fastapi import APIRouter, HTTPException, Request
from pydantic import BaseModel

from bigtiny.models.schedule import JobConfig
from bigtiny.scheduler.scheduler import Scheduler

logger = logging.getLogger(__name__)
router = APIRouter(prefix="/api/schedules")


class CreateScheduleRequest(BaseModel):
    name: str
    cron: str
    recipe_id: str
    parameters: dict = {}
    enabled: bool = True


class UpdateScheduleRequest(BaseModel):
    name: str | None = None
    cron: str | None = None
    recipe_id: str | None = None
    parameters: dict | None = None
    enabled: bool | None = None


@router.get("")
async def list_schedules(request: Request):
    db = request.app.state.db
    rows = await db.fetch_all("SELECT * FROM schedule_jobs ORDER BY created_at DESC")
    return {"jobs": [dict(r) for r in rows]}


@router.post("")
async def create_schedule(body: CreateScheduleRequest, request: Request):
    scheduler: Scheduler = request.app.state.scheduler
    job_config = JobConfig(
        name=body.name,
        cron=body.cron,
        recipe_id=body.recipe_id,
        parameters=body.parameters,
        enabled=body.enabled,
    )
    job_id = await scheduler.add_job(job_config)
    return {"id": job_id, "status": "created"}


@router.post("/{job_id}/run_now")
async def run_schedule_now(job_id: str, request: Request):
    scheduler: Scheduler = request.app.state.scheduler
    db = request.app.state.db
    job = await db.fetch_one(
        "SELECT * FROM schedule_jobs WHERE id = :id", {"id": job_id}
    )
    if not job:
        raise HTTPException(404, "Scheduled job not found")
    try:
        await scheduler.run_job(job_id)
        return {"status": "triggered"}
    except Exception as e:
        raise HTTPException(500, detail=str(e))


@router.patch("/{job_id}")
async def update_schedule(job_id: str, body: UpdateScheduleRequest, request: Request):
    db = request.app.state.db
    existing = await db.fetch_one(
        "SELECT * FROM schedule_jobs WHERE id = :id", {"id": job_id}
    )
    if not existing:
        raise HTTPException(404, "Scheduled job not found")

    updates: dict[str, object] = {}
    if body.name is not None:
        updates["name"] = body.name
    if body.cron is not None:
        updates["cron"] = body.cron
    if body.recipe_id is not None:
        updates["recipe_id"] = body.recipe_id
    if body.parameters is not None:
        updates["parameters"] = json.dumps(body.parameters)
    if body.enabled is not None:
        updates["enabled"] = 1 if body.enabled else 0

    if updates:
        set_clause = ", ".join(f"{k} = :{k}" for k in updates)
        updates["id"] = job_id
        await db.execute(
            f"UPDATE schedule_jobs SET {set_clause}, updated_at = CURRENT_TIMESTAMP WHERE id = :id",
            updates,
        )

    return {"status": "updated"}


@router.delete("/{job_id}")
async def delete_schedule(job_id: str, request: Request):
    db = request.app.state.db
    existing = await db.fetch_one(
        "SELECT * FROM schedule_jobs WHERE id = :id", {"id": job_id}
    )
    if not existing:
        raise HTTPException(404, "Scheduled job not found")
    await db.execute("DELETE FROM schedule_jobs WHERE id = :id", {"id": job_id})
    return {"status": "deleted"}
