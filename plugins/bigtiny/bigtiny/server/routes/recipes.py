from __future__ import annotations

import json
import logging
from uuid import uuid4

from fastapi import APIRouter, HTTPException, Request
from pydantic import BaseModel

from bigtiny.recipes.engine import RecipeEngine

logger = logging.getLogger(__name__)
router = APIRouter(prefix="/api/recipes")


class CreateRecipeRequest(BaseModel):
    name: str
    prompt_template: str
    instructions: str | None = None
    parameters: list[str] | None = None
    required_mcp_servers: list[str] | None = None
    system_prompt_layer: str | None = None
    max_steps: int = 30


class ExecuteRecipeRequest(BaseModel):
    parameters: dict = {}


@router.get("")
async def list_recipes(request: Request):
    db = request.app.state.db
    rows = await db.fetch_all("SELECT * FROM recipes")
    return {"recipes": [dict(r) for r in rows]}


@router.post("")
async def create_recipe(body: CreateRecipeRequest, request: Request):
    db = request.app.state.db
    recipe_id = uuid4().hex[:8]
    await db.execute(
        "INSERT INTO recipes (id, name, prompt_template, instructions, parameters, "
        "required_mcp_servers, system_prompt_layer, max_steps) "
        "VALUES (:id, :name, :prompt, :instr, :params, :servers, :layer, :steps)",
        {
            "id": recipe_id,
            "name": body.name,
            "prompt": body.prompt_template,
            "instr": body.instructions,
            "params": json.dumps(body.parameters or []),
            "servers": json.dumps(body.required_mcp_servers or []),
            "layer": body.system_prompt_layer,
            "steps": body.max_steps,
        },
    )
    return {"id": recipe_id, "status": "created"}


@router.post("/{recipe_id}/execute")
async def execute_recipe(recipe_id: str, body: ExecuteRecipeRequest, request: Request):
    recipe_engine: RecipeEngine = request.app.state.recipe_engine
    try:
        session_id = await recipe_engine.execute(recipe_id, body.parameters)
        return {"session_id": session_id}
    except ValueError as e:
        raise HTTPException(404, detail=str(e))
    except Exception as e:
        raise HTTPException(500, detail=str(e))


@router.delete("/{recipe_id}")
async def delete_recipe(recipe_id: str, request: Request):
    db = request.app.state.db
    existing = await db.fetch_one(
        "SELECT * FROM recipes WHERE id = :id", {"id": recipe_id}
    )
    if not existing:
        raise HTTPException(404, "Recipe not found")
    await db.execute("DELETE FROM recipes WHERE id = :id", {"id": recipe_id})
    return {"status": "deleted"}
