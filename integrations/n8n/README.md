# snifrig alerts to Telegram (n8n)

`snifrig-alerts.json` is the n8n workflow "snifrig: alerts". It receives each alert a PC posts
with `snifrig webhook URL` and sends Josh one Telegram message, at most one per PC and alert
type per hour (repeats are counted in the next message). Executions are not saved.

What a PC posts: `{"source":"snifrig","host":"PC","t":"ISO time","key":"alert key","msg":"text"}`.
Anything else gets a 400.

## Set up

1. Import `snifrig-alerts.json` into n8n (or create it through the n8n API) after replacing
   `__SNIFRIG_ALERTS_SECRET__` with a long random string. That string is the shared secret:
   keep it out of git and chat. Locally it lives in `X:\snifrig\.env` as `SNIFRIG_ALERTS_SECRET`.
2. In the "Tell Josh" node choose the Telegram credential "Telegram (Josh)".
3. Activate the workflow and tag it `snifrig`.
4. On each PC: `snifrig webhook https://<host>/webhook/snifrig/alerts/<secret>`. It prints
   `test post: ok`, and one Telegram message arrives.

On ferryvps, n8n listens on localhost only: until the public HTTPS entry exists, a PC can only
post while `ssh -N ferryvps-ui` is open, using `http://localhost:5678/webhook/...`.

## Turn it off

Deactivate the workflow in n8n, or on a PC run `snifrig webhook off`.

Only alerts go to the VPS: never history files, machine IDs or license keys.
