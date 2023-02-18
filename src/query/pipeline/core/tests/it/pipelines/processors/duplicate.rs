// Copyright 2023 Datafuse Labs.
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

use common_exception::Result;
use common_expression::types::Int32Type;
use common_expression::DataBlock;
use common_expression::FromData;
use common_pipeline_core::processors::connect;
use common_pipeline_core::processors::port::InputPort;
use common_pipeline_core::processors::port::OutputPort;
use common_pipeline_core::processors::processor::Event;
use common_pipeline_core::processors::DuplicateProcessor;
use common_pipeline_core::processors::Processor;

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_duplicate_output_finish() -> Result<()> {
    {
        let input = InputPort::create();
        let output1 = OutputPort::create();
        let output2 = OutputPort::create();
        let mut processor =
            DuplicateProcessor::create(input.clone(), output1.clone(), output2.clone(), false);

        let upstream_output = OutputPort::create();
        let downstream_input1 = InputPort::create();
        let downstream_input2 = InputPort::create();

        unsafe {
            connect(&input, &upstream_output);
            connect(&downstream_input1, &output1);
            connect(&downstream_input2, &output2);
        }

        downstream_input1.set_need_data();
        downstream_input2.set_need_data();
        downstream_input1.finish();

        assert!(matches!(processor.event()?, Event::NeedData));

        downstream_input2.finish();
        assert!(matches!(processor.event()?, Event::Finished));
        assert!(input.is_finished());
    }

    {
        let input = InputPort::create();
        let output1 = OutputPort::create();
        let output2 = OutputPort::create();
        let mut processor =
            DuplicateProcessor::create(input.clone(), output1.clone(), output2.clone(), true);

        let upstream_output = OutputPort::create();
        let downstream_input1 = InputPort::create();
        let downstream_input2 = InputPort::create();

        unsafe {
            connect(&input, &upstream_output);
            connect(&downstream_input1, &output1);
            connect(&downstream_input2, &output2);
        }

        downstream_input1.finish();
        assert!(matches!(processor.event()?, Event::Finished));
        assert!(input.is_finished());
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_duplicate_processor() -> Result<()> {
    let input = InputPort::create();
    let output1 = OutputPort::create();
    let output2 = OutputPort::create();
    let mut processor =
        DuplicateProcessor::create(input.clone(), output1.clone(), output2.clone(), true);

    let upstream_output = OutputPort::create();
    let downstream_input1 = InputPort::create();
    let downstream_input2 = InputPort::create();

    unsafe {
        connect(&input, &upstream_output);
        connect(&downstream_input1, &output1);
        connect(&downstream_input2, &output2);
    }

    downstream_input1.set_need_data();
    downstream_input2.set_need_data();

    let col = Int32Type::from_data(vec![1, 2, 3]);
    let block = DataBlock::new_from_columns(vec![col.clone()]);
    upstream_output.push_data(Ok(block));
    assert!(matches!(processor.event()?, Event::NeedConsume));

    let out1 = downstream_input1.pull_data().unwrap()?;
    let out2 = downstream_input2.pull_data().unwrap()?;

    assert!(out1.columns()[0].value.as_column().unwrap().eq(&col));
    assert!(out2.columns()[0].value.as_column().unwrap().eq(&col));

    Ok(())
}