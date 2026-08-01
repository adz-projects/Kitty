from __future__ import annotations

import asyncio
import logging
import os
import sys
import time
from contextlib import asynccontextmanager

from fastapi import FastAPI

from bigtiny.config import load_config
from bigtiny.network import TailscaleClient
from bigtiny.storage import Database
from bigtiny.providers.router import ProviderRouter
from bigtiny.mcp.manager import MCPManager
from bigtiny.hitl.manager import HITLManager
from bigtiny.agent.context_manager import ContextManager
from bigtiny.agent.loop import Agent
from bigtiny.providers.summarizer_client import SummarizerClient
from bigtiny.recipes.engine import RecipeEngine
from bigtiny.scheduler.scheduler import Scheduler
from bigtiny.server.middleware import add_middleware
from bigtiny.server.routes.chat import router as chat_router
from bigtiny.server.routes.health import router as health_router
from bigtiny.server.routes.providers import router as providers_router
from bigtiny.server.routes.mcp import router as mcp_router
from bigtiny.server.routes.recipes import router as recipes_router
from bigtiny.server.routes.schedules import router as schedules_router

logger = logging.getLogger(__name__)


def loop_factory() -> asyncio.AbstractEventLoop:
    """Event-loop factory passed to uvicorn (loop="bigtiny.server.app:loop_factory").

    On Windows, asyncio subprocess support (required for stdio MCP servers)
    only exists on the Proactor loop. uvicorn's built-in factory switches to
    SelectorEventLoop in --reload mode, which breaks MCP server spawning
    with NotImplementedError — so we always pick the right loop ourselves.
    """
    if sys.platform == "win32":
        return asyncio.ProactorEventLoop()
    return asyncio.new_event_loop()


@asynccontextmanager
async def lifespan(app: FastAPI):
    # `--config`, if any, is relayed via this env var by __main__.py since
    # uvicorn invokes this module as a factory string ("bigtiny.server.app:
    # create_app") rather than passing the parsed args through directly.
    config = load_config(os.environ.get("BIGTINY_CONFIG_PATH"))

    db = Database()
    await db.connect()

    # Lazy — costs nothing at startup, degrades to a silent no-op if the
    # Tailscale daemon isn't running. Shared with MCPManager below so both
    # providers and remote MCP servers benefit from the same peer/resolved-
    # address cache instead of each maintaining their own.
    tailscale = TailscaleClient()

    mcp = MCPManager(db, tailscale=tailscale)
    await mcp.connect_all()

    router = ProviderRouter(db, tailscale=tailscale)
    await router.load_providers()

    hitl = HITLManager(db, config.hitl)
    context = ContextManager(
        db, config.token_management, config.summarizer.reserve_exchanges
    )
    summarizer = SummarizerClient(config.summarizer)
    agent = Agent(
        router,
        mcp,
        hitl,
        context,
        db,
        config.agent.max_concurrent_tool_calls,
        summarizer=summarizer,
        token_management_config=config.token_management,
        summarizer_config=config.summarizer,
    )

    recipe_engine = RecipeEngine(db, agent, mcp)
    scheduler = Scheduler(db, recipe_engine)

    app.state.db = db
    app.state.agent = agent
    app.state.mcp = mcp
    app.state.router = router
    app.state.hitl = hitl
    app.state.recipe_engine = recipe_engine
    app.state.scheduler = scheduler
    app.state.config = config
    app.state.startup_time = time.time()
    app.state.tailscale = tailscale

    await scheduler.start()

    yield

    await scheduler.stop()
    await agent.shutdown()
    await summarizer.aclose()
    await mcp.disconnect_all()
    await db.close()


def create_app() -> FastAPI:
    app = FastAPI(
        title="BigTiny",
        version="0.1.0",
        lifespan=lifespan,
    )

    import os
    add_middleware(app, secret=os.environ.get("BIGTINY_SECRET"))

    app.include_router(chat_router)
    app.include_router(health_router)
    app.include_router(providers_router)
    app.include_router(mcp_router)
    app.include_router(recipes_router)
    app.include_router(schedules_router)

    return app
