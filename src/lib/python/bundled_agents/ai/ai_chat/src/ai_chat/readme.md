# ai_chat

The AI agent apart from any provider: it keeps the conversation and completes each turn through the provider agent named by `provider_id` (`complete`). Grant it `can_call {<provider>: [complete, watch]}`; the provider admits it in `callable_by`. Model, endpoint and key are the provider's; the cache marks are this chat's (`caching`): the provider forwards them.
