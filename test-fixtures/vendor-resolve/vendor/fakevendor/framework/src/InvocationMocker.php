<?php

namespace FakeVendor\Framework;

/**
 * Invocation mocker returned by method().
 */
class InvocationMocker
{
    /**
     * Set the return value for this mock.
     *
     * @param mixed $value
     * @return self
     */
    public function willReturn($value): self
    {
        return $this;
    }

    /**
     * Set the expected arguments.
     *
     * @param mixed ...$args
     * @return self
     */
    public function with(...$args): self
    {
        return $this;
    }
}
