// Copyright 2021 Datafuse Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use databend_common_config::GlobalConfig;
use databend_common_exception::Result;
use databend_common_sql::plans::UnSettingPlan;

use crate::interpreters::Interpreter;
use crate::pipelines::PipelineBuildResult;
use crate::sessions::QueryAffect;
use crate::sessions::QueryContext;
use crate::sessions::TableContext;

pub struct UnSettingInterpreter {
    ctx: Arc<QueryContext>,
    set: UnSettingPlan,
}

impl UnSettingInterpreter {
    pub fn try_create(ctx: Arc<QueryContext>, set: UnSettingPlan) -> Result<Self> {
        Ok(UnSettingInterpreter { ctx, set })
    }
}

#[async_trait::async_trait]
impl Interpreter for UnSettingInterpreter {
    fn name(&self) -> &str {
        "SettingInterpreter"
    }

    #[async_backtrace::framed]
    async fn execute2(&self) -> Result<PipelineBuildResult> {
        let plan = self.set.clone();
        let mut keys: Vec<String> = vec![];
        let mut values: Vec<String> = vec![];
        let mut is_globals: Vec<bool> = vec![];

        let settings = self.ctx.get_shared_settings();
        for var in plan.vars {
            let (ok, value) = match var.to_lowercase().as_str() {
                // To be compatible with some drivers
                "sql_mode" | "autocommit" => (false, String::from("")),
                setting_key => {
                    // TODO(liyz): why drop the global setting without checking the variable is global or not?
                    self.ctx
                        .get_shared_settings()
                        .try_drop_global_setting(setting_key)
                        .await?;

                    let default_val = {
                        if setting_key == "max_memory_usage" {
                            let conf = GlobalConfig::instance();
                            if conf.query.max_server_memory_usage == 0 {
                                settings
                                    .check_and_get_default_value(setting_key)?
                                    .to_string()
                            } else {
                                conf.query.max_server_memory_usage.to_string()
                            }
                        } else if setting_key == "max_threads" {
                            let conf = GlobalConfig::instance();
                            if conf.query.num_cpus == 0 {
                                settings
                                    .check_and_get_default_value(setting_key)?
                                    .to_string()
                            } else {
                                conf.query.num_cpus.to_string()
                            }
                        } else {
                            settings
                                .check_and_get_default_value(setting_key)?
                                .to_string()
                        }
                    };
                    (true, default_val)
                }
            };
            if ok {
                // reset the current ctx settings, just remove it.
                self.ctx.get_shared_settings().unset_setting(&var);
                // set effect, this can be considered to be removed in the future.
                keys.push(var);
                values.push(value);
                is_globals.push(false);
            }
        }
        self.ctx.set_affect(QueryAffect::ChangeSettings {
            keys,
            values,
            is_globals,
        });

        Ok(PipelineBuildResult::create())
    }
}
