# Route reference fixture

Guidance for a web project, which is mostly claims about a service and not about
this disk. None of it is a dead reference, and none of these lines may ever be
deleted.

## Routes in prose

The API exposes GET /v1/sessions and POST /users/:id/refresh. Health check is
/healthz/live. Sign-in lives at /login.

## Routes that carry a file extension

The schema is published at /openapi.json and the bundle is served from
/static/app.js. Both are URLs the service answers, not files in this repository.

## Routes in a markdown link

Point people at [the guide](/docs/getting-started) before they file a bug.

## A real dead reference, for contrast

The parser used to live in src/gone.rs, which is a path and is not there.
