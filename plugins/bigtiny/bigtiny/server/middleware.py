from __future__ import annotations

import logging
import time

from fastapi import Request
from fastapi.responses import JSONResponse
from starlette.middleware.base import BaseHTTPMiddleware
from starlette.middleware.cors import CORSMiddleware

logger = logging.getLogger(__name__)


class APIKeyMiddleware(BaseHTTPMiddleware):
    """Requires X-API-Key on /api/* when a secret is configured.

    /api/health stays open so launchers can poll readiness before they
    have wired up auth.
    """

    def __init__(self, app, secret: str | None = None):
        super().__init__(app)
        self.secret = secret

    async def dispatch(self, request: Request, call_next):
        if (
            self.secret
            and request.url.path.startswith("/api")
            and request.url.path != "/api/health"
            and request.headers.get("x-api-key") != self.secret
        ):
            return JSONResponse(
                status_code=401,
                content={"error": "Unauthorized", "detail": "Missing or invalid X-API-Key"},
            )
        return await call_next(request)


class ErrorHandlingMiddleware(BaseHTTPMiddleware):
    async def dispatch(self, request: Request, call_next):
        try:
            response = await call_next(request)
            return response
        except Exception as e:
            logger.exception("Unhandled error: %s %s", request.method, request.url.path)
            return JSONResponse(
                status_code=500,
                content={"error": "Internal server error", "detail": str(e)},
            )


class RequestLoggingMiddleware(BaseHTTPMiddleware):
    async def dispatch(self, request: Request, call_next):
        start = time.monotonic()
        response = await call_next(request)
        duration = (time.monotonic() - start) * 1000
        logger.info(
            "%s %s -> %d (%.1fms)",
            request.method,
            request.url.path,
            response.status_code,
            duration,
        )
        return response


def add_middleware(app, secret: str | None = None):
    app.add_middleware(
        CORSMiddleware,
        allow_origins=["*"],
        allow_credentials=True,
        allow_methods=["*"],
        allow_headers=["*"],
    )
    app.add_middleware(ErrorHandlingMiddleware)
    app.add_middleware(APIKeyMiddleware, secret=secret)
    app.add_middleware(RequestLoggingMiddleware)
