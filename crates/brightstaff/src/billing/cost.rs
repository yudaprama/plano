use common::configuration::ModelPricing;
use std::collections::HashMap;

/// Breakdown of costs for a single LLM request.
#[derive(Debug, Clone)]
pub struct CostBreakdown {
    pub input_cost: f64,
    pub output_cost: f64,
    pub total_cost: f64,
    /// Credits to deduct (total_cost * 1_000_000 stored as integer to avoid float precision issues in Redis).
    pub credits_deducted: i64,
    pub usage_source: UsageSource,
    pub input_per_million: f64,
    pub output_per_million: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSource {
    Reported,
    Estimated,
}

impl UsageSource {
    pub fn as_str(self) -> &'static str {
        match self {
            UsageSource::Reported => "reported",
            UsageSource::Estimated => "estimated",
        }
    }
}

/// Token usage extracted from the LLM response.
#[derive(Debug, Clone, Default)]
pub struct TokenUsage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub cached_input_tokens: i64,
    pub reasoning_tokens: i64,
}

impl TokenUsage {
    pub fn is_empty(&self) -> bool {
        self.total_tokens == 0 && self.prompt_tokens == 0 && self.completion_tokens == 0
    }
}

/// Calculate cost from reported token usage and the pricing matrix.
pub fn calculate_cost(
    usage: &TokenUsage,
    model: &str,
    pricing: &HashMap<String, ModelPricing>,
    default_pricing: &ModelPricing,
) -> CostBreakdown {
    let rate = pricing.get(model).unwrap_or(default_pricing);

    let non_cached_input = (usage.prompt_tokens - usage.cached_input_tokens).max(0) as f64;
    let cached_input = usage.cached_input_tokens as f64;
    let output = usage.completion_tokens as f64;

    let input_cost = (non_cached_input * rate.input_per_million / 1_000_000.0)
        + (cached_input * rate.input_per_million * rate.cache_discount / 1_000_000.0);
    let output_cost = output * rate.output_per_million / 1_000_000.0;
    let total_cost = input_cost + output_cost;
    let credits_deducted = (total_cost * 1_000_000.0).round() as i64;

    CostBreakdown {
        input_cost,
        output_cost,
        total_cost,
        credits_deducted,
        usage_source: UsageSource::Reported,
        input_per_million: rate.input_per_million,
        output_per_million: rate.output_per_million,
    }
}

/// Calculate an estimated cost when the response lacks a `usage` block.
pub fn calculate_estimated_cost(
    model: &str,
    pricing: &HashMap<String, ModelPricing>,
    default_pricing: &ModelPricing,
) -> CostBreakdown {
    let rate = pricing.get(model).unwrap_or(default_pricing);
    let estimated = rate.estimated_default_tokens as f64;

    let input_cost = estimated * rate.input_per_million / 1_000_000.0;
    let output_cost = estimated * rate.output_per_million / 1_000_000.0;
    let total_cost = input_cost + output_cost;
    let credits_deducted = (total_cost * 1_000_000.0).round() as i64;

    CostBreakdown {
        input_cost,
        output_cost,
        total_cost,
        credits_deducted,
        usage_source: UsageSource::Estimated,
        input_per_million: rate.input_per_million,
        output_per_million: rate.output_per_million,
    }
}

pub fn estimated_usage(
    model: &str,
    pricing: &HashMap<String, ModelPricing>,
    default_pricing: &ModelPricing,
) -> TokenUsage {
    let rate = pricing.get(model).unwrap_or(default_pricing);
    let estimated = rate.estimated_default_tokens;

    TokenUsage {
        prompt_tokens: estimated,
        completion_tokens: estimated,
        total_tokens: estimated * 2,
        cached_input_tokens: 0,
        reasoning_tokens: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::configuration::ModelPricing;

    fn test_pricing() -> (HashMap<String, ModelPricing>, ModelPricing) {
        let mut pricing = HashMap::new();
        pricing.insert(
            "openai/gpt-4o".to_string(),
            ModelPricing {
                input_per_million: 2.50,
                output_per_million: 10.00,
                cache_discount: 0.5,
                estimated_default_tokens: 500,
            },
        );
        let default = ModelPricing {
            input_per_million: 5.0,
            output_per_million: 15.0,
            cache_discount: 0.5,
            estimated_default_tokens: 500,
        };
        (pricing, default)
    }

    #[test]
    fn test_calculate_cost_known_model() {
        let (pricing, default) = test_pricing();
        let usage = TokenUsage {
            prompt_tokens: 1000,
            completion_tokens: 500,
            total_tokens: 1500,
            cached_input_tokens: 200,
            reasoning_tokens: 0,
        };
        let cost = calculate_cost(&usage, "openai/gpt-4o", &pricing, &default);
        // non-cached: 800 * 2.50 / 1M = 0.002
        // cached: 200 * 2.50 * 0.5 / 1M = 0.00025
        // output: 500 * 10.0 / 1M = 0.005
        assert!((cost.input_cost - 0.00225).abs() < 1e-10);
        assert!((cost.output_cost - 0.005).abs() < 1e-10);
        assert!((cost.total_cost - 0.00725).abs() < 1e-10);
        assert_eq!(cost.usage_source, UsageSource::Reported);
    }

    #[test]
    fn test_calculate_cost_unknown_model_uses_default() {
        let (pricing, default) = test_pricing();
        let usage = TokenUsage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            cached_input_tokens: 0,
            reasoning_tokens: 0,
        };
        let cost = calculate_cost(&usage, "unknown/model", &pricing, &default);
        // input: 100 * 5.0 / 1M = 0.0005
        // output: 50 * 15.0 / 1M = 0.00075
        assert!((cost.total_cost - 0.00125).abs() < 1e-10);
    }

    #[test]
    fn test_calculate_estimated_cost() {
        let (pricing, default) = test_pricing();
        let cost = calculate_estimated_cost("openai/gpt-4o", &pricing, &default);
        assert_eq!(cost.usage_source, UsageSource::Estimated);
        assert!(cost.total_cost > 0.0);
        // 500 tokens estimated for both input and output
        // input: 500 * 2.50 / 1M, output: 500 * 10.0 / 1M
        assert!((cost.total_cost - 0.00625).abs() < 1e-10);
    }

    #[test]
    fn test_estimated_usage_uses_model_default() {
        let (pricing, default) = test_pricing();
        let usage = estimated_usage("openai/gpt-4o", &pricing, &default);
        assert_eq!(usage.prompt_tokens, 500);
        assert_eq!(usage.completion_tokens, 500);
        assert_eq!(usage.total_tokens, 1000);
    }

    #[test]
    fn test_credits_deducted_is_integer() {
        let (pricing, default) = test_pricing();
        let usage = TokenUsage {
            prompt_tokens: 1_000_000,
            completion_tokens: 1_000_000,
            total_tokens: 2_000_000,
            cached_input_tokens: 0,
            reasoning_tokens: 0,
        };
        let cost = calculate_cost(&usage, "openai/gpt-4o", &pricing, &default);
        // input: 1M * 2.50 / 1M = 2.50, output: 1M * 10.0 / 1M = 10.0
        // credits = 12.50 * 1M = 12_500_000
        assert_eq!(cost.credits_deducted, 12_500_000);
    }
}
