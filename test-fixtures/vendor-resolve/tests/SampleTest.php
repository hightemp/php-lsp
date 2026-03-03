<?php

namespace App\Tests;

use FakeVendor\Framework\TestCase;
use FakeVendor\Framework\MockBuilder;
use App\TimerService;

/**
 * Test class that exercises vendor class resolution.
 *
 * Case 1: $this->createStub() — createStub is defined in BaseAssert (grandparent).
 *         TestCase extends BaseAssert. Our class extends TestCase.
 *         Requires: vendor lazy-load + recursive parent loading.
 *
 * Case 2: $this->timerMock->method('start') — method() is on MockBuilder.
 *         timerMock property is typed as MockBuilder.
 *         Requires: cross-file type resolution for property type.
 *
 * Case 3: $this->setUp() — setUp is on TestCase (direct parent, vendor).
 *         Requires: vendor lazy-load.
 *
 * Case 4: Chained: $this->createStub(TimerService::class)->method('start')
 *         createStub returns 'object' — can't resolve further (expected null).
 */
class SampleTest extends TestCase
{
    private MockBuilder $timerMock;
    private TimerService $timerService;

    protected function setUp(): void
    {
        parent::setUp();
        $this->timerMock = new MockBuilder();
        $this->timerService = new TimerService();
    }

    public function testCreateStub(): void
    {
        // Case 1: go-to-definition on "createStub" (line 41, col ~16)
        $stub = $this->createStub(TimerService::class);
    }

    public function testMethodOnMock(): void
    {
        // Case 2: go-to-definition on "method" (line 47, col ~27)
        $this->timerMock->method('start');
    }

    public function testSetUp(): void
    {
        // Case 3: go-to-definition on "setUp" (line 52, col ~16)
        $this->setUp();
    }

    public function testTimerServiceDirect(): void
    {
        // Case 4: go-to-definition on "start" (line 58, col ~31)
        $this->timerService->start('benchmark');
    }
}
