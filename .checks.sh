set -uo pipefail
cd webviews
F="src/agent-session/acpmux/App.tsx src/agent-session/acpmux/model.ts src/agent-session/acpmux/conversation/turns.ts src/agent-session/acpmux/conversation/chatFind.ts src/agent-session/acpmux/conversation/WaitingMessagesCard.tsx src/agent-session/acpmux/conversation/waitingMessages.ts test/waiting-messages-card.test.tsx"
echo ===FMT; ./node_modules/.bin/vp fmt $F >/dev/null 2>&1; git diff --stat -- .; git diff -- . | head -80
echo ===TSC; bun run typecheck 2>&1 | tail -12
echo ===LINT; bun run lint 2>&1 | tail -6
echo ===CAT; bun run check:catalog-keys 2>&1 | tail -4
echo ===TESTS; bun test test/waiting-messages-card.test.tsx test/queue-agent-message.test.tsx test/agent-pane-pending-steer.test.tsx test/agent-pane-queue.test.ts test/agent-pane-steer-take-back.test.tsx test/transcript-steer-continuous.test.tsx test/steer-no-caption.test.tsx test/queue-row-actions.test.tsx test/chat-find.test.ts test/chat-find-matches.test.ts test/transcript-find-follow.test.tsx test/tool-failure-find.test.tsx test/agent-pane-steer.test.ts test/agent-pane-turns*.test.ts test/turn*.test.ts 2>&1 | grep -E "^\(fail\)|^ *[0-9]+ (pass|fail)|^Ran|error:|Expected|Received" | head -40
echo ===END
