# check=skip=FromPlatformFlagConstDisallowed
# Pin amd64: the Tektii engine runs linux/amd64, so an arm64 image built on
# Apple Silicon would only fail at run time with a cryptic exec-format error.
FROM --platform=linux/amd64 python:3.12-slim

WORKDIR /app

COPY pyproject.toml strategy.py ./
RUN pip install --no-cache-dir .

# Run as an unprivileged user. Not required by Cloud Run but worth teaching.
RUN useradd --system --no-create-home appuser
USER appuser

ENV PYTHONUNBUFFERED=1

# Exec form so python is PID 1 and receives SIGTERM directly on `docker stop`.
CMD ["python", "strategy.py"]
