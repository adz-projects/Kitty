from __future__ import annotations

import json
import logging
from pathlib import Path
from typing import Any, Awaitable, Callable
from uuid import uuid4

import yaml
from jinja2 import Environment, FileSystemLoader

from bigtiny import paths
from bigtiny.agent.loop import Agent
from bigtiny.mcp.manager import MCPManager
from bigtiny.models.mcp_server import ToolDefinition
from bigtiny.server.events import SSEEvent
from bigtiny.storage import Database

logger = logging.getLogger(__name__)


async def _noop_callback(event: SSEEvent) -> None:
    pass


class RecipeEngine:
    def __init__(
        self,
        db: Database,
        agent: Agent,
        mcp: MCPManager,
        recipes_dir: str | None = None,
    ):
        self.db = db
        self.agent = agent
        self.mcp = mcp
        recipes_dir = recipes_dir if recipes_dir is not None else str(Path(paths.data_dir()) / "recipes")
        self._recipes_dir = recipes_dir
        expanded = str(Path(recipes_dir).expanduser())
        self.jinja = Environment(
            loader=FileSystemLoader(expanded),
            autoescape=False,
        )

    async def load_recipes_from_directory(self, directory: str | None = None) -> int:
        target = directory or self._recipes_dir
        target_path = Path(target).expanduser()
        if not target_path.is_dir():
            logger.warning("Recipe directory not found: %s", target)
            return 0

        count = 0
        for fpath in sorted(target_path.glob("*.yaml")) + sorted(target_path.glob("*.yml")):
            try:
                raw = fpath.read_text()
                data = yaml.safe_load(raw)
                if not data or not isinstance(data, dict):
                    continue

                recipe_id = data.get("id", uuid4().hex[:8])
                await self.db.execute(
                    """INSERT OR REPLACE INTO recipes
                       (id, name, prompt_template, instructions, parameters,
                        required_mcp_servers, system_prompt_layer, max_steps)
                       VALUES (:id, :name, :prompt, :instr, :params, :servers, :layer, :steps)""",
                    {
                        "id": recipe_id,
                        "name": data.get("name", fpath.stem),
                        "prompt": data.get("prompt_template", ""),
                        "instr": data.get("instructions"),
                        "params": json.dumps(data.get("parameters", [])),
                        "servers": json.dumps(data.get("required_mcp_servers", [])),
                        "layer": data.get("system_prompt_layer"),
                        "steps": data.get("max_steps", 30),
                    },
                )
                count += 1
            except Exception as e:
                logger.warning("Failed to load recipe %s: %s", fpath, e)

        return count

    async def execute(
        self,
        recipe_id: str,
        parameters: dict[str, Any],
        event_callback: Callable[[SSEEvent], Awaitable[None]] | None = None,
    ) -> str:
        recipe = await self.db.fetch_one(
            "SELECT * FROM recipes WHERE id = :id", {"id": recipe_id}
        )
        if not recipe:
            raise ValueError(f"Recipe {recipe_id} not found")

        callback = event_callback or _noop_callback

        prompt = self.jinja.from_string(recipe["prompt_template"]).render(**parameters)
        instructions_text = recipe.get("instructions")
        if instructions_text:
            instructions_text = self.jinja.from_string(instructions_text).render(**parameters)

        # The agent reads persona_override from session metadata, so the
        # recipe's instructions/system_prompt_layer must be stored there
        # to take effect.
        persona_parts = []
        if instructions_text:
            persona_parts.append(instructions_text)
        if recipe.get("system_prompt_layer"):
            persona_parts.append(f"You are a {recipe['system_prompt_layer']}.")

        metadata: dict[str, Any] = {
            "recipe_id": recipe_id,
            "parameters": parameters,
            "recipe_name": recipe["name"],
        }
        if persona_parts:
            metadata["persona_override"] = "\n".join(persona_parts)

        session_id = uuid4().hex
        await self.db.execute(
            "INSERT INTO sessions (id, name, metadata) VALUES (:id, :name, :meta)",
            {"id": session_id, "name": recipe["name"], "meta": json.dumps(metadata)},
        )

        required_servers = json.loads(recipe["required_mcp_servers"] or "[]")
        for server_name in required_servers:
            server = await self.db.fetch_one(
                "SELECT * FROM mcp_servers WHERE name = :name",
                {"name": server_name},
            )
            if server:
                try:
                    await self.mcp.connect_server(server["id"])
                except Exception as e:
                    logger.warning("Failed to connect MCP server '%s': %s", server_name, e)

        await self.agent.run(
            session_id=session_id,
            user_message=prompt,
            event_callback=callback,
        )

        return session_id
