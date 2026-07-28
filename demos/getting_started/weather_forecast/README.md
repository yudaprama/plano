# Function calling

This demo shows how you can use Plano's core function calling capabilities.

# Starting the demo

1. Please make sure the [pre-requisites](https://github.com/katanemo/arch/?tab=readme-ov-file#prerequisites) are installed correctly
2. Start Plano

3. ```sh
   sh run_demo.sh
   ```
4. Test with curl:
   ```sh
   curl http://localhost:10000/v1/chat/completions \
     -H "Content-Type: application/json" \
     -d '{"model": "gpt-4o", "messages": [{"role": "user", "content": "how is the weather in San Francisco?"}]}'
   ```

Here is a sample interaction,
<img width="575" alt="image" src="https://github.com/user-attachments/assets/e0929490-3eb2-4130-ae87-a732aea4d059">

## Using the Chat UI and Tracing (optional)

To start AnythingLLM (chat UI) and other optional services, pass `--with-ui`:

```sh
sh run_demo.sh --with-ui
```

- Navigate to http://localhost:3001/ for AnythingLLM
- Navigate to http://localhost:16686/ for Jaeger tracing UI

## Billing & Usage Tracking (optional)

This demo includes billing configuration (currently disabled). To enable usage-based billing:

1. Set up Redis for billing (see [Billing Guide](../../../docs/source/guides/billing.rst))
2. Set environment variables:
   ```sh
   export REDIS_URL="redis://localhost:6379"
   export TALOS_URL="https://your-talos-instance.com"  # Optional
   export TALOS_ADMIN_TOKEN="your-token"  # Optional
   ```
3. Enable billing in `config.yaml`:
   ```yaml
   billing:
     enabled: true
   ```
5. Set initial user balance in Redis:
   ```sh
   redis-cli SET "plano:billing:balance:user_123" 10000000  # $10.00
   ```

For detailed documentation, see the [Billing & Usage Tracking Guide](../../../docs/source/guides/billing.rst).

### Stopping Demo

1. To end the demo, run the following command:
   ```sh
   sh run_demo.sh down
   ```
