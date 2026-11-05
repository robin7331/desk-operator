-- Power path for a send-only device (e.g. a reverse-engineered RF remote) that only reacts
-- reliably when the command is sent twice, 50 ms apart. There is no state() function, so desk
-- reports the result as "assumed" and refuses to toggle.
--
--   power = { script = "scripts/double_press.lua", params = {
--     remote = { provider = "sim", endpoint = "blind_lamp" },
--   } }

local function press(ctx, on)
  local remote = ctx.power(ctx.params.remote)
  remote:set(on) -- raw send: no read-back, no idempotence check
  ctx.sleep(50)
  remote:set(on)
end

function on(ctx)
  press(ctx, true)
end

function off(ctx)
  press(ctx, false)
end
