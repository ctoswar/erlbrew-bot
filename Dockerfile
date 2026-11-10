FROM python:3.12-slim

ENV PYTHONDONTWRITEBYTECODE=1 \
    PYTHONUNBUFFERED=1 \
    PIP_NO_CACHE_DIR=1

WORKDIR /app

COPY requirements.txt ./
RUN pip install --no-cache-dir -r requirements.txt

# App code + bundled fallback copies of faq.yaml / knowledge.md
COPY main.py config.py db.py messenger.py faq.py events.py llm.py notify.py orders.py menu.py ./
COPY faq.yaml knowledge.md ./

# Non-root user; ./data is expected to be a bind-mounted volume for the live-editable
# faq.yaml / knowledge.md / erlbrew.db
RUN useradd --system --no-create-home bot \
    && mkdir -p /app/data \
    && chown -R bot:bot /app
USER bot

EXPOSE 8000

HEALTHCHECK --interval=30s --timeout=5s --start-period=10s --retries=2 \
    CMD python -c "import urllib.request,sys;\
r=urllib.request.urlopen('http://127.0.0.1:8000/health', timeout=4);\
sys.exit(0 if r.status==200 else 1)" || exit 1

# `serve` runs uvicorn (single worker, low memory); `unpause <PSID>` is the CLI.
CMD ["python", "main.py", "serve"]
