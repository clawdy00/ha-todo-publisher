# Home Assistant Todo Publisher

A small push-based Rust web service for publishing individual Home Assistant to-do lists without giving consumers any Home Assistant credentials.

Home Assistant pushes one to-do list snapshot to a slug URL such as `/api/todos/cars`. Consumers read the same slug URL with a separate read secret.

## Model

Each published to-do list has a URL slug:

```text
cars
shopping
chores
```

The slug is the access-control boundary. A write secret for `cars` can only write `/api/todos/cars`; a read secret for `cars` can only read `/api/todos/cars`.

There are no namespaces and no aggregate read endpoint.

## Configuration

Configuration is TOML. By default the app reads `config.toml` in the working directory. Override with:

```sh
CONFIG_PATH=/config/config.toml
```

Example:

```toml
bind_addr = "0.0.0.0:8080"

[todos.cars]
write_token = "replace-with-generated-cars-write-secret"
read_token = "replace-with-generated-cars-read-secret"

[todos.shopping]
write_token = "replace-with-generated-shopping-write-secret"
read_token = "replace-with-generated-shopping-read-secret"
```

Rules:

- `bind_addr` is optional and defaults to `0.0.0.0:8080`.
- Each `[todos.<slug>]` creates exactly one published list.
- Slugs must be 1-64 characters of lowercase letters, digits, `-`, or `_`.
- Each `write_token` and `read_token` must be at least 24 characters.
- The slug is not part of the bearer value sent by clients; it is only the config key that binds a secret to a URL.

Generate secrets:

```sh
openssl rand -base64 32
```

Run locally:

```sh
cat > config.toml <<'EOF'
bind_addr = "127.0.0.1:8080"

[todos.cars]
write_token = "replace-with-generated-cars-write-secret"
read_token = "replace-with-generated-cars-read-secret"
EOF

cargo run
```

## API

The service is JSON-only. It has no human-facing HTML page.

### Write one list

```http
POST /api/todos/cars
Authorization: Bearer CARS_WRITE_SECRET
Content-Type: application/json
```

Body:

```json
{
  "id": "todo.cars",
  "name": "Cars",
  "items": [
    {
      "id": "item-1",
      "summary": "Book service",
      "status": "needs_action",
      "due": null,
      "description": null,
      "url": null
    }
  ]
}
```

`updated_at` may be included in the request body, but is optional. If omitted, the server uses the receive time.

Optional stronger write authentication is available with HMAC:

```http
X-HA-Signature-256: sha256=<hex hmac sha256 over raw request body using that slug's write secret>
```

If this header is present, it is verified instead of bearer auth. Bearer mode is the practical default for Home Assistant YAML because stock templates do not conveniently calculate HMAC.

### Read one list

```http
GET /api/todos/cars
Authorization: Bearer CARS_READ_SECRET
```

Response:

```json
{
  "slug": "cars",
  "updated_at": "2026-01-01T00:00:00Z",
  "list": {
    "id": "todo.cars",
    "name": "Cars",
    "items": [
      {
        "id": "item-1",
        "summary": "Book service",
        "status": "needs_action",
        "due": null,
        "description": null,
        "url": null
      }
    ]
  }
}
```

A read secret for another slug returns `401` for this slug, because secrets are per slug.

### Health

```http
GET /healthz
```

## Home Assistant automation

This example publishes a Home Assistant to-do entity called `todo.cars` to the URL slug `cars` whenever it changes. Replace the URL, entity id, and display name for your setup. Store the cars write secret in Home Assistant `secrets.yaml`.

`secrets.yaml`:

```yaml
ha_todo_publisher_cars_write_secret: "generate-a-long-random-secret"
```

`configuration.yaml`:

```yaml
rest_command:
  publish_todos_cars:
    url: "https://todo.example.internal/api/todos/cars"
    method: POST
    content_type: "application/json"
    headers:
      authorization: "Bearer {{ secret }}"
    payload: "{{ payload }}"
```

Automation:

```yaml
automation:
  - alias: "Publish cars todo list snapshot"
    mode: restart
    trigger:
      - platform: state
        entity_id: todo.cars
    action:
      - service: todo.get_items
        target:
          entity_id: todo.cars
        data:
          status: needs_action
        response_variable: todo_response
      - service: rest_command.publish_todos_cars
        data:
          secret: !secret ha_todo_publisher_cars_write_secret
          payload: >-
            {
              "id": "todo.cars",
              "name": "Cars",
              "items": [
                {%- for item in todo_response['todo.cars']['items'] | default([]) -%}
                {
                  "id": {{ (item.uid | default(item.summary, true)) | to_json }},
                  "summary": {{ item.summary | to_json }},
                  "status": {{ (item.status | default('needs_action', true)) | to_json }},
                  "due": {{ (item.due | default(none)) | to_json }},
                  "description": {{ (item.description | default(none)) | to_json }},
                  "url": null
                }{{ "," if not loop.last else "" }}
                {%- endfor -%}
              ]
            }
```

## Kubernetes sketch

Kubernetes manifests are provided under `deploy/`:

- `namespace.yaml`
- `secret.example.yaml`
- `deployment.yaml`
- `service.yaml`
- `ingress.example.yaml`

The config file contains secrets, so the example stores it in a Kubernetes Secret and mounts it read-only.

```yaml
apiVersion: v1
kind: Secret
metadata:
  name: ha-todo-publisher-config
type: Opaque
stringData:
  config.toml: |
    bind_addr = "0.0.0.0:8080"

    [todos.cars]
    write_token = "replace-with-generated-cars-write-secret"
    read_token = "replace-with-generated-cars-read-secret"
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: ha-todo-publisher
spec:
  replicas: 1
  selector:
    matchLabels:
      app: ha-todo-publisher
  template:
    metadata:
      labels:
        app: ha-todo-publisher
    spec:
      containers:
        - name: app
          image: ghcr.io/clawdy00/ha-todo-publisher:latest
          ports:
            - containerPort: 8080
          env:
            - name: CONFIG_PATH
              value: /config/config.toml
          volumeMounts:
            - name: config
              mountPath: /config
              readOnly: true
          readinessProbe:
            httpGet:
              path: /healthz
              port: 8080
          livenessProbe:
            httpGet:
              path: /healthz
              port: 8080
      volumes:
        - name: config
          secret:
            secretName: ha-todo-publisher-config
```

Add a Service and Ingress for your local cluster hostname.

## Security notes

- Do not commit real to-do payloads, secrets, screenshots, or Home Assistant hostnames if private.
- Use separate write and read secrets per slug.
- Rotate a secret by editing the mounted TOML config and restarting the pod.
- The service keeps state in memory only; restart means Home Assistant must push again before data appears.
