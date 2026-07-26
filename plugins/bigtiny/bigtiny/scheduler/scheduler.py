from __future__ import annotations

import json
import logging
from uuid import uuid4

from apscheduler.schedulers.asyncio import AsyncIOScheduler
from apscheduler.triggers.cron import CronTrigger

from bigtiny.models.schedule import JobConfig
from bigtiny.recipes.engine import RecipeEngine
from bigtiny.storage import Database

logger = logging.getLogger(__name__)


class Scheduler:
    def __init__(self, db: Database, recipe_engine: RecipeEngine):
        self.db = db
        self.recipe_engine = recipe_engine
        self._apscheduler = AsyncIOScheduler()

    async def start(self) -> None:
        jobs = await self.db.fetch_all(
            "SELECT * FROM schedule_jobs WHERE enabled = 1"
        )
        for job in jobs:
            try:
                self._apscheduler.add_job(
                    self._execute_job,
                    CronTrigger.from_crontab(job["cron"]),
                    args=[job["id"]],
                    id=job["id"],
                    replace_existing=True,
                    name=job["name"],
                )
            except Exception as e:
                logger.warning("Failed to schedule job %s: %s", job["id"], e)
        self._apscheduler.start()
        logger.info("Scheduler started with %d jobs", len(jobs))

    async def add_job(self, job_config: JobConfig) -> str:
        job_id = uuid4().hex[:8]
        await self.db.execute(
            "INSERT INTO schedule_jobs (id, name, cron, recipe_id, parameters, enabled) "
            "VALUES (:id, :name, :cron, :recipe_id, :params, :enabled)",
            {
                "id": job_id,
                "name": job_config.name,
                "cron": job_config.cron,
                "recipe_id": job_config.recipe_id,
                "params": json.dumps(job_config.parameters or {}),
                "enabled": 1 if job_config.enabled else 0,
            },
        )
        self._apscheduler.add_job(
            self._execute_job,
            CronTrigger.from_crontab(job_config.cron),
            args=[job_id],
            id=job_id,
            replace_existing=True,
            name=job_config.name,
        )
        return job_id

    async def run_job(self, job_id: str) -> None:
        job = await self.db.fetch_one(
            "SELECT * FROM schedule_jobs WHERE id = :id", {"id": job_id}
        )
        if not job:
            raise ValueError(f"Scheduled job {job_id} not found")
        await self._execute_job(job_id)

    async def _execute_job(self, job_id: str) -> None:
        job = await self.db.fetch_one(
            "SELECT * FROM schedule_jobs WHERE id = :id", {"id": job_id}
        )
        if not job:
            return

        exec_id = uuid4().hex[:8]
        temp_sid = f"_job_{exec_id}"
        await self.db.execute(
            "INSERT INTO sessions (id, name, status) VALUES (:id, :name, 'idle')",
            {"id": temp_sid, "name": f"scheduled:{job_id}"},
        )
        await self.db.execute(
            "INSERT INTO execution_history (id, session_id, trigger_type, trigger_id, status) "
            "VALUES (:id, :sid, 'schedule', :trigger, 'running')",
            {"id": exec_id, "sid": temp_sid, "trigger": job_id},
        )

        try:
            parameters = json.loads(job["parameters"] or "{}")
            session_id = await self.recipe_engine.execute(job["recipe_id"], parameters)
            await self.db.execute(
                "UPDATE execution_history SET status = 'completed', "
                "session_id = :sid, completed_at = CURRENT_TIMESTAMP WHERE id = :id",
                {"sid": session_id, "id": exec_id},
            )
            await self.db.execute(
                "DELETE FROM sessions WHERE id = :id", {"id": temp_sid},
            )
        except Exception as e:
            logger.exception("Scheduled job %s failed", job_id)
            await self.db.execute(
                "UPDATE execution_history SET status = 'failed', "
                "error_message = :err, completed_at = CURRENT_TIMESTAMP WHERE id = :id",
                {"err": str(e), "id": exec_id},
            )
            # Can't delete temp_sid here the way the success path does above:
            # execution_history.session_id (NOT NULL, REFERENCES sessions(id),
            # no ON DELETE clause) still points at it on this path — with
            # foreign_keys=ON (storage.py's connect()), that delete would
            # raise a FOREIGN KEY constraint failure inside this except
            # block, which is strictly worse than leaving the row (it would
            # mask the real failure being logged above). Mark it failed
            # instead so it's visibly a dead marker rather than a
            # perpetually-'idle' ghost session — genuine deletion would
            # require either relaxing this FK to ON DELETE SET NULL/CASCADE
            # or deleting the execution_history audit row too, both bigger
            # changes than this fix warrants.
            await self.db.execute(
                "UPDATE sessions SET status = 'failed' WHERE id = :id",
                {"id": temp_sid},
            )

    async def stop(self) -> None:
        self._apscheduler.shutdown()
        logger.info("Scheduler stopped")
