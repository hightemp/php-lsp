<?php

namespace FakeVendor\Framework;

/**
 * Mock builder interface.
 */
class MockBuilder
{
    /**
     * Configure a method mock. Returns InvocationMocker for chaining.
     *
     * @param string $methodName
     * @return InvocationMocker
     */
    public function method(string $methodName): InvocationMocker
    {
        return new InvocationMocker();
    }
}
