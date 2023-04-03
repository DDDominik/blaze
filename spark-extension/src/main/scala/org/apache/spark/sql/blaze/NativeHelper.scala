/*
 * Copyright 2022 The Blaze Authors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package org.apache.spark.sql.blaze

import scala.collection.immutable.TreeMap
import scala.language.reflectiveCalls

import org.apache.hadoop.security.UserGroupInformation
import org.apache.spark.Partition
import org.apache.spark.SparkConf
import org.apache.spark.SparkContext
import org.apache.spark.SparkEnv
import org.apache.spark.TaskContext
import org.blaze.protobuf.PhysicalPlanNode

import org.apache.spark.internal.Logging
import org.apache.spark.internal.config
import org.apache.spark.sql.catalyst.InternalRow
import org.apache.spark.sql.execution.SparkPlan
import org.apache.spark.sql.execution.blaze.arrowio.ColumnarHelper
import org.apache.spark.sql.execution.metric.SQLMetric
import org.apache.spark.sql.execution.metric.SQLMetrics
import org.apache.spark.sql.vectorized.ColumnarBatch
import org.apache.spark.storage.DiskBlockManager

object NativeHelper extends Logging {
  val currentUser: UserGroupInformation = UserGroupInformation.getCurrentUser
  private val conf: SparkConf = SparkEnv.get.conf

  val batchSize: Int = conf.getInt("spark.blaze.batchSize", 10000)
  val nativeMemory: Long = {
    val MEMORY_OVERHEAD_FACTOR = 0.10
    val MEMORY_OVERHEAD_MIN = 384L
    val executorMemory = conf.get(config.EXECUTOR_MEMORY)
    val executorMemoryOverheadMiB = conf
      .get(config.EXECUTOR_MEMORY_OVERHEAD)
      .getOrElse(math.max((MEMORY_OVERHEAD_FACTOR * executorMemory).toLong, MEMORY_OVERHEAD_MIN))
    executorMemoryOverheadMiB * 1024L * 1024L
  }
  val memoryFraction: Double = conf.getDouble("spark.blaze.memoryFraction", 0.4);

  def isNative(exec: SparkPlan): Boolean =
    Shims.get.sparkPlanShims.isNative(exec)

  def getUnderlyingNativePlan(exec: SparkPlan): NativeSupports =
    Shims.get.sparkPlanShims.getUnderlyingNativePlan(exec)

  def executeNative(exec: SparkPlan): NativeRDD =
    Shims.get.sparkPlanShims.executeNative(exec)

  def executeNativePlan(
      nativePlan: PhysicalPlanNode,
      metrics: MetricNode,
      partition: Partition,
      context: TaskContext): Iterator[InternalRow] = {

    executeNativePlanColumnar(nativePlan, metrics, partition, context)
      .flatMap(ColumnarHelper.batchAsRowIter)
  }

  def executeNativePlanColumnar(
      nativePlan: PhysicalPlanNode,
      metrics: MetricNode,
      partition: Partition,
      context: TaskContext): Iterator[ColumnarBatch] = {

    BlazeCallNativeWrapper(nativePlan, partition, context, metrics).getBatchIterator
  }

  def getDefaultNativeMetrics(sc: SparkContext): Map[String, SQLMetric] =
    TreeMap(
      "output_rows" -> SQLMetrics.createMetric(sc, "Native.output_rows"),
      "output_batches" -> SQLMetrics.createMetric(sc, "Native.output_batches"),
      "input_rows" -> SQLMetrics.createMetric(sc, "Native.input_rows"),
      "input_batches" -> SQLMetrics.createMetric(sc, "Native.input_batches"),
      "elapsed_compute" -> SQLMetrics.createNanoTimingMetric(sc, "Native.elapsed_compute"),
      "join_time" -> SQLMetrics.createNanoTimingMetric(sc, "Native.join_time"),
      "spilled_bytes" -> SQLMetrics.createSizeMetric(sc, "Native.spilled_bytes"))
}
